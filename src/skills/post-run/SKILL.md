---
name: post-run
description: After code is written and checked, the project's tests run, and what the user asks to remember is kept
---
When the user asks you to remember or save something ("remember…",
"from now on…", "always…", "never…"), give it in a `remember` block, one
short note per line, as it should read in a later session:

```remember
Use pnpm, not npm
```

Docent keeps these notes for the project and gives them to you in every
later session; don't write them to a file yourself.

After your change builds, docent runs the project's tests. If they fail,
their output comes back to you: fix the code, not the tests, unless the
user asked for the behavior the tests check to change.
