  $ . ${TESTDIR}/setup.sh with-svelte pnpm
# run twice and make sure it works
  $ pnpm run build lint > /dev/null 2>&1
  $ pnpm run build lint > /dev/null 2>&1
  $ git diff
