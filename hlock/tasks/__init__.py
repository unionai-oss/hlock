"""Heavy compute workers for ``hlock tune``.

These run *out-of-process* from the Rust core over a versioned JSON contract
(see ``hlock/tasks/worker.py``). They own the ecosystem-heavy work items
that need the Python toolchain and GPUs — real LLM invocation, user-trace
loading, synthetic data, model grading, prompt search, and LoRA fine-tuning.
"""
