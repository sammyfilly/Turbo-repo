use std::{
    borrow::Cow,
    cmp::min,
    fmt,
    io::{self, BufRead, Read, Result as IoResult, Write},
    mem,
    ops::{AddAssign, Deref},
    pin::Pin,
    sync::Arc,
    task::{Context as TaskContext, Poll},
};

use anyhow::{Context, Result};
use bytes::Bytes;
use futures::Stream;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use tokio::io::{AsyncRead, ReadBuf};
use turbo_tasks_hash::{DeterministicHash, DeterministicHasher};
use RopeElem::{Local, Shared};

static EMPTY_BUF: &[u8] = &[];

/// A Rope provides an efficient structure for sharing bytes/strings between
/// multiple sources. Cloning a Rope is extremely cheap (Arc and usize), and
/// sharing the contents of one Rope can be done by just cloning an Arc.
///
/// Ropes are immutable, in order to construct one see [RopeBuilder].
#[turbo_tasks::value(shared, serialization = "custom", eq = "manual")]
#[derive(Clone, Debug, Default)]
pub struct Rope {
    /// Total length of all held bytes.
    length: usize,

    /// A shareable container holding the rope's bytes.
    #[turbo_tasks(debug_ignore, trace_ignore)]
    data: InnerRope,
}

/// An Arc container for ropes. This indirection allows for easily sharing the
/// contents between Ropes (and also RopeBuilders/RopeReaders).
#[derive(Clone, Debug)]
struct InnerRope(Arc<[RopeElem]>);

/// Differentiates the types of stored bytes in a rope.
#[derive(Clone, Debug)]
enum RopeElem {
    /// Local bytes are owned directly by this rope.
    Local(Bytes),

    /// Shared holds the Arc container of another rope.
    Shared(InnerRope),
}

/// RopeBuilder provides a mutable container to append bytes/strings. This can
/// also append _other_ Rope instances cheaply, allowing efficient sharing of
/// the contents without a full clone of the bytes.
#[derive(Default, Debug)]
pub struct RopeBuilder {
    /// Total length of all previously committed bytes.
    length: usize,

    /// Immutable bytes references that have been appended to this builder. The
    /// rope is the combination of all these committed bytes.
    committed: Vec<RopeElem>,

    /// Stores bytes that have been pushed, but are not yet committed. This is
    /// either an attempt to push a static lifetime, or a push of owned bytes.
    /// When the builder is flushed, we will commit these bytes into a real
    /// Bytes instance.
    uncommitted: Uncommitted,
}

/// Stores any bytes which have been pushed, but we haven't decided to commit
/// yet. Uncommitted bytes allow us to build larger buffers out of possibly
/// small pushes.
#[derive(Default)]
enum Uncommitted {
    #[default]
    None,

    /// Stores our attempt to push static lifetime bytes into the rope. If we
    /// build the Rope or concatenate another Rope, we can commit a static
    /// Bytes reference and save memory. If not, we'll concatenate this into
    /// writable bytes to be committed later.
    Static(&'static [u8]),

    /// Mutable bytes collection where non-static/non-shared bytes are written.
    /// This builds until the next time a static or shared bytes is
    /// appended, in which case we split the buffer and commit. Finishing
    /// the builder also commits these bytes.
    Owned(Vec<u8>),
}

impl Rope {
    pub fn len(&self) -> usize {
        self.length
    }

    pub fn is_empty(&self) -> bool {
        self.length == 0
    }

    /// Returns a [Read]/[AsyncRead]/[Iterator] instance over all bytes.
    pub fn read(&self) -> RopeReader<'static> {
        RopeReader::new(self)
    }

    /// Returns a String instance of all bytes.
    pub fn to_str(&self) -> Result<Cow<'_, str>> {
        self.data.to_str()
    }
}

impl<T: Into<Bytes>> From<T> for Rope {
    fn from(bytes: T) -> Self {
        let bytes = bytes.into();
        // We can't have an InnerRope which contains an empty Local section.
        if bytes.is_empty() {
            Default::default()
        } else {
            Rope {
                length: bytes.len(),
                data: InnerRope(Arc::from([Local(bytes)])),
            }
        }
    }
}

impl RopeBuilder {
    /// Push owned bytes into the Rope.
    ///
    /// If possible use [push_static_bytes] or `+=` operation instead, as they
    /// will create a reference to shared memory instead of cloning the bytes.
    pub fn push_bytes(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }

        self.uncommitted.push_bytes(bytes);
    }

    /// Push static lifetime bytes into the Rope.
    ///
    /// This is more efficient than pushing owned bytes, because the internal
    /// data does not need to be copied when the rope is read.
    pub fn push_static_bytes(&mut self, bytes: &'static [u8]) {
        if bytes.is_empty() {
            return;
        }

        // If the string is smaller than the cost of a Bytes reference (4 usizes), then
        // it's more efficient to own the bytes in a new buffer. We may be able to reuse
        // that buffer when more bytes are pushed.
        if bytes.len() < mem::size_of::<Bytes>() {
            return self.uncommitted.push_static_bytes(bytes);
        }

        // We may have pending bytes from a prior push.
        self.finish();

        self.length += bytes.len();
        self.committed.push(Local(bytes.into()));
    }

    /// Concatenate another Rope instance into our builder.
    ///
    /// This is much more efficient than pushing actual bytes, since we can
    /// share the other Rope's references without copying the underlying data.
    pub fn concat(&mut self, other: &Rope) {
        if other.is_empty() {
            return;
        }

        // We may have pending bytes from a prior push.
        self.finish();

        self.length += other.len();
        self.committed.push(Shared(other.data.clone()));
    }

    /// Writes any pending bytes into our committed queue.
    ///
    /// This may be called multiple times without issue.
    pub fn finish(&mut self) {
        if let Some(b) = self.uncommitted.finish() {
            debug_assert!(!b.is_empty(), "must not have empty uncommitted bytes");
            self.length += b.len();
            self.committed.push(Local(b));
        }
    }

    pub fn len(&self) -> usize {
        self.length + self.uncommitted.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Constructs our final, immutable Rope instance.
    pub fn build(mut self) -> Rope {
        self.finish();
        Rope {
            length: self.length,
            data: InnerRope::from(self.committed),
        }
    }
}

impl From<&'static str> for RopeBuilder {
    default fn from(bytes: &'static str) -> Self {
        let mut r = RopeBuilder::default();
        r += bytes;
        r
    }
}

impl From<Vec<u8>> for RopeBuilder {
    fn from(bytes: Vec<u8>) -> Self {
        RopeBuilder {
            // Directly constructing the Uncommitted allows us to skip copying the bytes.
            uncommitted: Uncommitted::from(bytes),
            ..Default::default()
        }
    }
}

impl Write for RopeBuilder {
    fn write(&mut self, bytes: &[u8]) -> IoResult<usize> {
        self.push_bytes(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> IoResult<()> {
        self.finish();
        Ok(())
    }
}

impl AddAssign<&'static str> for RopeBuilder {
    /// Pushes a reference to static memory onto the rope.
    ///
    /// This is more efficient than pushing owned bytes, because the internal
    /// data does not need to be copied when the rope is read.
    fn add_assign(&mut self, rhs: &'static str) {
        self.push_static_bytes(rhs.as_bytes());
    }
}

impl AddAssign<&Rope> for RopeBuilder {
    fn add_assign(&mut self, rhs: &Rope) {
        self.concat(rhs);
    }
}

impl Uncommitted {
    fn len(&self) -> usize {
        match self {
            Uncommitted::None => 0,
            Uncommitted::Static(s) => s.len(),
            Uncommitted::Owned(v) => v.len(),
        }
    }

    /// Pushes owned bytes, converting the current representation to an Owned if
    /// it's not already.
    fn push_bytes(&mut self, bytes: &[u8]) {
        debug_assert!(!bytes.is_empty(), "must not push empty uncommitted bytes");
        match self {
            Self::None => *self = Self::Owned(bytes.to_vec()),
            Self::Static(s) => {
                // If we'd previously pushed static bytes, we instead concatenate those bytes
                // with the new bytes in an attempt to use less memory rather than committing 2
                // Bytes references (2 * 4 usizes).
                let v = [s, bytes].concat();
                *self = Self::Owned(v);
            }
            Self::Owned(v) => v.extend(bytes),
        }
    }

    /// Pushes static lifetime bytes, but only if the current representation is
    /// None. Else, it coverts to an Owned.
    fn push_static_bytes(&mut self, bytes: &'static [u8]) {
        debug_assert!(!bytes.is_empty(), "must not push empty uncommitted bytes");
        match self {
            // If we've not already pushed static bytes, we attempt to store the bytes for later. If
            // we push owned bytes or another static bytes, then this attempt will fail and we'll
            // instead concatenate into a single owned Bytes. But if we don't push anything (build
            // the Rope), or concatenate another Rope (we can't join our bytes with the InnerRope of
            // another Rope), we'll be able to commit a static Bytes reference and save overall
            // memory (a small static Bytes reference is better than a small owned Bytes reference).
            Self::None => *self = Self::Static(bytes),
            _ => self.push_bytes(bytes),
        }
    }

    /// Converts the current uncommitted bytes into a Bytes, resetting our
    /// representation to None.
    fn finish(&mut self) -> Option<Bytes> {
        match mem::take(self) {
            Self::None => None,
            Self::Static(s) => Some(s.into()),
            Self::Owned(v) => Some(v.into()),
        }
    }
}

impl fmt::Debug for Uncommitted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Uncommitted::None => f.write_str("None"),
            Uncommitted::Static(s) => f
                .debug_tuple("Static")
                .field(&Bytes::from_static(s))
                .finish(),
            Uncommitted::Owned(v) => f
                .debug_tuple("Owned")
                .field(&Bytes::from(v.clone()))
                .finish(),
        }
    }
}

impl DeterministicHash for Rope {
    /// Ropes with similar contents hash the same, regardless of their
    /// structure.
    fn deterministic_hash<H: DeterministicHasher>(&self, state: &mut H) {
        state.write_usize(self.len());
        self.data.deterministic_hash(state);
    }
}

impl Serialize for Rope {
    /// Ropes are always serialized into contiguous strings, because
    /// deserialization won't deduplicate and share the Arcs (being the only
    /// possible owner of a individual "shared" data doesn't make sense).
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::Error;
        let s = self.to_str().map_err(Error::custom)?;
        serializer.serialize_str(&s)
    }
}

impl<'de> Deserialize<'de> for Rope {
    /// Deserializes strings into a contiguous, immutable Rope.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let bytes = <Vec<u8>>::deserialize(deserializer)?;
        Ok(Rope::from(bytes))
    }
}

impl PartialEq for Rope {
    // Ropes with similar contents are equals, regardless of their structure.
    fn eq(&self, other: &Self) -> bool {
        if Arc::ptr_eq(&self.data, &other.data) {
            return true;
        }
        if self.len() != other.len() {
            return false;
        }

        // Fast path for structurally equal Ropes. With this, we can do memory reference
        // checks and skip some contents equality.
        let left = &self.data;
        let right = &other.data;
        let len = min(left.len(), right.len());
        let mut index = 0;
        while index < len {
            let a = &left[index];
            let b = &right[index];

            match a.maybe_eq(b) {
                // Bytes or InnerRope point to the same memory, or Bytes are contents equal.
                Some(true) => index += 1,
                // Bytes are not contents equal.
                Some(false) => return false,
                // InnerRopes point to different memory, or the Ropes weren't structurally equal.
                None => break,
            }
        }
        // If we reach the end of iteration without finding a mismatch (or early
        // breaking), then we know the ropes are either equal or not equal.
        if index == len {
            // We know that any remaining RopeElem in the InnerRope must contain content, so
            // if either one contains more RopeElem than they cannot be equal.
            return left.len() == right.len();
        }

        // At this point, we need to do slower contents equality. It's possible we'll
        // still get some memory reference equality for Bytes.
        let mut left = RopeReader::new_borrow(left, index);
        let mut right = RopeReader::new_borrow(right, index);
        loop {
            match (left.fill_buf(), right.fill_buf()) {
                // fill_buf should always return Ok, with either some number of bytes or 0 bytes
                // when consumed.
                (Ok(a), Ok(b)) => {
                    let len = min(a.len(), b.len());

                    // When one buffer is consumed, both must be consumed.
                    if len == 0 {
                        return a.len() == b.len();
                    }

                    if a[0..len] != b[0..len] {
                        return false;
                    }

                    left.consume(len);
                    right.consume(len);
                }

                // If an error is ever returned (which shouldn't happen for us) for either/both,
                // then we can't prove equality.
                _ => return false,
            }
        }
    }
}

impl Eq for Rope {}

impl From<Vec<u8>> for Uncommitted {
    fn from(bytes: Vec<u8>) -> Self {
        if bytes.is_empty() {
            Uncommitted::None
        } else {
            Uncommitted::Owned(bytes)
        }
    }
}

impl InnerRope {
    /// Returns a String instance of all bytes.
    pub fn to_str(&self) -> Result<Cow<'_, str>> {
        match &self[..] {
            [] => Ok(Cow::Borrowed("")),
            [Shared(inner)] => inner.to_str(),
            [Local(bytes)] => {
                let utf8 = std::str::from_utf8(bytes);
                utf8.context("failed to convert rope into string")
                    .map(Cow::Borrowed)
            }
            _ => {
                let mut read = RopeReader::new_borrow(self, 0);
                let mut string = String::with_capacity(self.len());
                let res = read.read_to_string(&mut string);
                res.context("failed to convert rope into string")?;
                Ok(Cow::Owned(string))
            }
        }
    }
}

impl Default for InnerRope {
    fn default() -> Self {
        InnerRope(Arc::from([]))
    }
}

impl DeterministicHash for InnerRope {
    /// Ropes with similar contents hash the same, regardless of their
    /// structure. Notice the InnerRope does not contain a length (and any
    /// shared InnerRopes won't either), so the exact structure isn't
    /// relevant at this point.
    fn deterministic_hash<H: DeterministicHasher>(&self, state: &mut H) {
        for v in self.0.iter() {
            v.deterministic_hash(state);
        }
    }
}

impl From<Vec<RopeElem>> for InnerRope {
    fn from(els: Vec<RopeElem>) -> Self {
        if cfg!(debug_assertions) {
            // It's important that an InnerRope never contain an empty Bytes section.
            for el in els.iter() {
                match el {
                    Local(b) => debug_assert!(!b.is_empty(), "must not have empty Bytes"),
                    Shared(s) => {
                        // We check whether the shared slice is empty, and not its elements. The
                        // only way to construct the Shared's InnerRope is
                        // in this mod, and we have already checked that
                        // none of its elements are empty.
                        debug_assert!(!s.is_empty(), "must not have empty InnerRope");
                    }
                }
            }
        }
        InnerRope(Arc::from(els))
    }
}

impl Deref for InnerRope {
    type Target = Arc<[RopeElem]>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl RopeElem {
    fn maybe_eq(&self, other: &Self) -> Option<bool> {
        match (self, other) {
            (Local(a), Local(b)) => {
                if a.len() == b.len() {
                    return Some(a == b);
                }

                // But if not, the rope may still be contents equal if a following section
                // contains the missing bytes.
                None
            }
            (Shared(a), Shared(b)) => {
                if Arc::ptr_eq(&a.0, &b.0) {
                    return Some(true);
                }

                // But if not, they might still be equal and we need to fallback to slower
                // equality.
                None
            }
            _ => None,
        }
    }
}

impl DeterministicHash for RopeElem {
    /// Ropes with similar contents hash the same, regardless of their
    /// structure. Notice the Bytes length is not hashed, and shared InnerRopes
    /// do not contain a length.
    fn deterministic_hash<H: DeterministicHasher>(&self, state: &mut H) {
        match self {
            Local(bytes) => state.write_bytes(bytes),
            Shared(inner) => inner.deterministic_hash(state),
        }
    }
}

/// Implements the [Read]/[AsyncRead]/[Iterator] trait over a [Rope].
#[derive(Debug)]
pub struct RopeReader<'a> {
    /// The Rope's tree is kept as a cloned stack, allowing us to accomplish
    /// incremental yielding.
    stack: Vec<StackElem<'a>>,

    /// The held root allows us to invert the ownership of Bytes. By storing a
    /// copy of Rope's root, we can ensure that our borrowed StackElems will
    /// live at least as long as this RopeReader.
    held_root: Option<InnerRope>,
}

/// A StackElem holds the current index into either a Bytes or a shared Rope.
/// When the index reaches the end of the associated data, it is removed and we
/// continue onto the next item in the stack.
#[derive(Debug)]
enum StackElem<'a> {
    Local(&'a Bytes, usize),
    Shared(&'a InnerRope, usize),
}

impl<'a> RopeReader<'a> {
    /// Constructs a new RopeReader, taking ownership of the Rope's internal
    /// memory and freeing us from the lifetime constraints of the Rope.
    fn new(rope: &Rope) -> RopeReader<'static> {
        // SAFETY: We transmute the InnerRope into a 'static lifetime so that it is
        // usable without keeping a reference to the Rope alive. This is safe because
        // the RopeReader itself holds an Arc clone of the data, ensuring the data will
        // live at least as long as the reader.
        let borrow = unsafe { std::mem::transmute::<&InnerRope, &'static InnerRope>(&rope.data) };
        let mut reader = RopeReader::new_borrow(borrow, 0);
        reader.held_root = Some(borrow.clone());
        reader
    }

    /// Constructs a new RopeReader without taking ownership of the Rope's
    /// internal memory.
    fn new_borrow(inner: &'a InnerRope, index: usize) -> Self {
        let stack = if index >= inner.len() {
            vec![]
        } else {
            let mut v = Vec::with_capacity(2);
            v.push(StackElem::Shared(inner, index));
            v
        };
        RopeReader {
            stack,
            held_root: None,
        }
    }

    /// Iterates over the rope's elements recursively until we find the next
    /// Local section, returning its Bytes.
    fn next_internal(&mut self) -> Option<(&'a Bytes, usize)> {
        loop {
            match self.stack.last_mut() {
                None => return None,
                Some(StackElem::Local(b, o)) => {
                    debug_assert!(*o < b.len(), "must not have empty Bytes section");
                    return Some((b, *o));
                }
                Some(StackElem::Shared(inner, index)) => {
                    let el = &inner[*index];
                    if *index + 1 < inner.len() {
                        *index += 1;
                    } else {
                        self.stack.pop();
                    }

                    self.stack.push(StackElem::from(el));
                }
            };
        }
    }

    /// A shared implementation for reading bytes. This takes the basic
    /// operations needed for both Read and AsyncRead.
    fn read_internal(&mut self, want: usize, buf: &mut ReadBuf<'_>) -> usize {
        let mut remaining = want;

        while remaining > 0 {
            let bytes = match self.next_internal() {
                None => break,
                Some((b, o)) => &b[o..],
            };

            let amt = min(bytes.len(), remaining);
            buf.put_slice(&bytes[0..amt]);
            self.consume(amt);
            remaining -= amt;
        }

        want - remaining
    }
}

impl<'a> Iterator for RopeReader<'a> {
    type Item = &'a Bytes;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_internal().map(|(bytes, offset)| {
            debug_assert!(offset == 0, "cannot mix Iterator with Read/BufRead");
            self.stack.pop();
            bytes
        })
    }
}

impl<'a> Read for RopeReader<'a> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        Ok(self.read_internal(buf.len(), &mut ReadBuf::new(buf)))
    }
}

impl<'a> AsyncRead for RopeReader<'a> {
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        this.read_internal(buf.remaining(), buf);
        Poll::Ready(Ok(()))
    }
}

impl<'a> BufRead for RopeReader<'a> {
    fn fill_buf(&mut self) -> IoResult<&[u8]> {
        Ok(match self.next_internal() {
            None => EMPTY_BUF,
            Some((bytes, offset)) => &bytes[offset..],
        })
    }

    fn consume(&mut self, amt: usize) {
        if let Some(StackElem::Local(bytes, offset)) = self.stack.last_mut() {
            if *offset + amt < bytes.len() {
                *offset += amt;
            } else {
                debug_assert!(
                    *offset + amt == bytes.len(),
                    "cannot over-consume RopeReader"
                );
                self.stack.pop();
            }
        }
    }
}

impl<'a> Stream for RopeReader<'a> {
    // The Result<Bytes> item type is required for this to be streamable into a
    // [Hyper::Body].
    type Item = Result<Bytes>;

    // Returns a "result" of reading the next shared bytes reference. This
    // differs from [Read::read] by not copying any memory.
    fn poll_next(self: Pin<&mut Self>, _cx: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        Poll::Ready(this.next().cloned().map(Ok))
    }
}

impl<'a> From<&'a RopeElem> for StackElem<'a> {
    fn from(el: &'a RopeElem) -> Self {
        match el {
            Local(bytes) => Self::Local(bytes, 0),
            Shared(inner) => Self::Shared(inner, 0),
        }
    }
}

#[cfg(test)]
mod test {
    use std::{
        cmp::min,
        io::{BufRead, Read},
    };

    use bytes::Bytes;

    use super::{InnerRope, Rope, RopeBuilder, RopeElem};

    // These are intentionally not exposed, because they do inefficient conversions
    // in order to fully test cases.
    impl From<&str> for RopeElem {
        fn from(value: &str) -> Self {
            RopeElem::Local(value.to_string().into())
        }
    }
    impl From<Vec<RopeElem>> for RopeElem {
        fn from(value: Vec<RopeElem>) -> Self {
            RopeElem::Shared(InnerRope::from(value))
        }
    }
    impl From<Rope> for RopeElem {
        fn from(value: Rope) -> Self {
            RopeElem::Shared(value.data)
        }
    }
    impl Rope {
        fn new(value: Vec<RopeElem>) -> Self {
            let data = InnerRope::from(value);
            Rope {
                length: data.len(),
                data,
            }
        }
    }
    impl InnerRope {
        fn len(&self) -> usize {
            self.iter().map(|v| v.len()).sum()
        }
    }
    impl RopeElem {
        fn len(&self) -> usize {
            match self {
                RopeElem::Local(b) => b.len(),
                RopeElem::Shared(r) => r.len(),
            }
        }
    }

    #[test]
    fn empty_build_without_pushes() {
        let empty = RopeBuilder::default().build();
        let mut reader = empty.read();
        assert!(reader.next().is_none());
    }

    #[test]
    fn empty_build_with_empty_static_push() {
        let mut builder = RopeBuilder::default();
        builder += "";

        let empty = builder.build();
        let mut reader = empty.read();
        assert!(reader.next().is_none());
    }

    #[test]
    fn empty_build_with_empty_bytes_push() {
        let mut builder = RopeBuilder::default();
        builder.push_bytes(&[]);

        let empty = builder.build();
        let mut reader = empty.read();
        assert!(reader.next().is_none());
    }

    #[test]
    fn empty_build_with_empty_concat() {
        let mut builder = RopeBuilder::default();
        builder += &RopeBuilder::default().build();

        let empty = builder.build();
        let mut reader = empty.read();
        assert!(reader.next().is_none());
    }

    #[test]
    fn empty_from_empty_static_str() {
        let empty = Rope::from("");
        let mut reader = empty.read();
        assert!(reader.next().is_none());
    }

    #[test]
    fn empty_from_empty_string() {
        let empty = Rope::from("".to_string());
        let mut reader = empty.read();
        assert!(reader.next().is_none());
    }

    #[test]
    fn empty_equality() {
        let a = Rope::from("");
        let b = Rope::from("");

        assert_eq!(a, b);
    }

    #[test]
    fn cloned_equality() {
        let a = Rope::from("abc");
        let b = a.clone();

        assert_eq!(a, b);
    }

    #[test]
    fn value_equality() {
        let a = Rope::from("abc".to_string());
        let b = Rope::from("abc".to_string());

        assert_eq!(a, b);
    }

    #[test]
    fn value_inequality() {
        let a = Rope::from("abc".to_string());
        let b = Rope::from("def".to_string());

        assert_ne!(a, b);
    }

    #[test]
    fn value_equality_shared_1() {
        let shared = Rope::from("def");
        let a = Rope::new(vec!["abc".into(), shared.clone().into(), "ghi".into()]);
        let b = Rope::new(vec!["abc".into(), shared.into(), "ghi".into()]);

        assert_eq!(a, b);
    }

    #[test]
    fn value_equality_shared_2() {
        let a = Rope::new(vec!["abc".into(), vec!["def".into()].into(), "ghi".into()]);
        let b = Rope::new(vec!["abc".into(), vec!["def".into()].into(), "ghi".into()]);

        assert_eq!(a, b);
    }

    #[test]
    fn value_equality_splits_1() {
        let a = Rope::new(vec!["a".into(), "aa".into()]);
        let b = Rope::new(vec!["aa".into(), "a".into()]);

        assert_eq!(a, b);
    }

    #[test]
    fn value_equality_splits_2() {
        let a = Rope::new(vec![vec!["a".into()].into(), "aa".into()]);
        let b = Rope::new(vec![vec!["aa".into()].into(), "a".into()]);

        assert_eq!(a, b);
    }

    #[test]
    fn value_inequality_shared_1() {
        let shared = Rope::from("def");
        let a = Rope::new(vec!["aaa".into(), shared.clone().into(), "ghi".into()]);
        let b = Rope::new(vec!["bbb".into(), shared.into(), "ghi".into()]);

        assert_ne!(a, b);
    }

    #[test]
    fn value_inequality_shared_2() {
        let a = Rope::new(vec!["abc".into(), vec!["ddd".into()].into(), "ghi".into()]);
        let b = Rope::new(vec!["abc".into(), vec!["eee".into()].into(), "ghi".into()]);

        assert_ne!(a, b);
    }

    #[test]
    fn value_inequality_shared_3() {
        let shared = Rope::from("def");
        let a = Rope::new(vec!["abc".into(), shared.clone().into(), "ggg".into()]);
        let b = Rope::new(vec!["abc".into(), shared.into(), "hhh".into()]);

        assert_ne!(a, b);
    }

    #[test]
    fn iteration() {
        let shared = Rope::from("def");
        let rope = Rope::new(vec!["abc".into(), shared.into(), "ghi".into()]);

        let chunks = rope.read().into_iter().collect::<Vec<&Bytes>>();

        assert_eq!(chunks, vec!["abc", "def", "ghi"]);
    }

    #[test]
    fn read() {
        let shared = Rope::from("def");
        let rope = Rope::new(vec!["abc".into(), shared.into(), "ghi".into()]);

        let mut chunks = vec![];
        let mut buf = [0_u8; 2];
        let mut reader = rope.read();
        loop {
            let amt = reader.read(&mut buf).unwrap();
            if amt == 0 {
                break;
            }
            chunks.push(Vec::from(&buf[0..amt]));
        }

        assert_eq!(
            chunks,
            vec![
                Vec::from(*b"ab"),
                Vec::from(*b"cd"),
                Vec::from(*b"ef"),
                Vec::from(*b"gh"),
                Vec::from(*b"i")
            ]
        );
    }

    #[test]
    fn fill_buf() {
        let shared = Rope::from("def");
        let rope = Rope::new(vec!["abc".into(), shared.into(), "ghi".into()]);

        let mut chunks = vec![];
        let mut reader = rope.read();
        loop {
            let buf = reader.fill_buf().unwrap();
            if buf.is_empty() {
                break;
            }
            let c = min(2, buf.len());
            chunks.push(Vec::from(buf));
            reader.consume(c);
        }

        assert_eq!(
            chunks,
            // We're receiving a full buf, then only consuming 2 bytes, so we'll still get the
            // third.
            vec![
                Vec::from(*b"abc"),
                Vec::from(*b"c"),
                Vec::from(*b"def"),
                Vec::from(*b"f"),
                Vec::from(*b"ghi"),
                Vec::from(*b"i")
            ]
        );
    }
}
