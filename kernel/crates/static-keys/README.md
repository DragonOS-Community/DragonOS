# static-keys (DragonOS maintained copy)

This crate is based on upstream `static-keys` 0.8.2 and retains its MIT OR
Apache-2.0 license. DragonOS keeps a local copy because the upstream
`CodeManipulator` API cannot express an expected instruction, a batch
transaction, or an error before publishing the key state.

The DragonOS delta is intentionally limited to the code-patch transaction
traits and update ordering. Architecture instruction generation and inline
assembly remain upstream code.
