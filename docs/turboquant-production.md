# Production Considerations

Notes on running Dyson with self-hosted models in production.

---

## Economics

Self-hosting means paying for GPU time.  The cost equation has three levers:
model size, quantization, and hardware utilization.

### TurboQuant

TurboQuant (mixed-precision weight quantization with per-channel scaling) is
the highest-leverage optimization.  It reduces weight precision — typically
FP16 → INT4 — while preserving output quality through outlier-aware rounding.

The tradeoff is direct: smaller weights free VRAM for either **longer context**
or **more concurrent users**.

| Format | Bits | VRAM (70B) | Quality | Fits on |
|--------|------|------------|---------|---------|
| FP16 | 16 | ~140 GB | Baseline | 2×A100-80GB |
| INT8 | 8 | ~70 GB | Near-lossless | 1×A100-80GB |
| INT4 (AWQ) | 4 | ~40 GB | Minor degradation | 1×A100-80GB or 2×A6000 |

A 70B model at INT4 fits where an FP16 copy cannot.  The freed VRAM is
available for KV cache, which means longer agent conversations before OOM — or
more concurrent requests at the same context length.  For Dyson agents running
multi-turn tool loops, this matters: a single task can easily consume 16k–32k
tokens of context.

**Recommendation:** INT4 with group quantization (group size 128) covers most
Dyson deployments.  Tool calling and structured output are robust at this
precision.  Step up to INT8 if you see malformed tool call JSON.

### Serving a quantized model

Dyson connects via the OpenAI-compatible provider.  Point it at your serving
backend:

```json
{
  "agent": {
    "provider": "openai",
    "model": "llama-3.1-70b-instruct-awq",
    "base_url": "http://localhost:8000",
    "api_key": "not-needed",
    "max_tokens": 16384
  }
}
```

vLLM with AWQ:

```bash
vllm serve meta-llama/Llama-3.1-70B-Instruct-AWQ \
  --quantization awq \
  --max-model-len 32768 \
  --tensor-parallel-size 2
```

Ollama with GGUF quantization:

```bash
ollama run llama3.1:70b-instruct-q4_K_M
```

### Sizing

- **VRAM budget:** model weights + KV cache + activations.  Leave 30–50% free
  after loading weights.
- **Context vs. concurrency:** maximize `--max-model-len` for longer agent
  loops, or reduce it and raise `--max-num-seqs` for more users.
- **Validation:** test tool calling at your chosen quantization level before
  deploying.  Long contexts and multi-step reasoning are most sensitive to
  precision loss.

---

See also: [LLM Clients](llm-clients.md) ·
[Configuration](configuration.md)
