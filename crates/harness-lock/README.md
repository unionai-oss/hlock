# harness-lock

**`harness-lock` is an alias for [`hlock`](https://crates.io/crates/hlock).**

hlock makes your agent harness pinnable, portable, and tunable: pin its models, MCP
servers, skills, and config in a `harness.lock`, rehydrate it anywhere, then tune it
against your own tasks.

This crate has no code of its own. Like `hlock`, it re-exports
[`harness-lock-core`](https://crates.io/crates/harness-lock-core) at the same version,
under `harness_lock::…`. New projects should depend on the primary name:

```bash
cargo add hlock
```

- 📖 **Docs:** <https://docs.hlock.ai/> · <https://docs.rs/hlock>
- 💻 **Source:** <https://github.com/unionai-oss/hlock>
- 🐍 **Python:** `pip install hlock`
