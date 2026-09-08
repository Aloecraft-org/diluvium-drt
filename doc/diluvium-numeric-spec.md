# Diluvium Numeric: staged spec

Status: draft for handoff. Names in this document (`array`, connector and package names) are placeholders unless marked settled.

## 1. What this is

Diluvium gains a typed array value and a small set of numeric kernels, layered so that GPU execution, scikit-shaped ML, pandas-shaped dataframes, and parquet I/O all arrive as additions outside the core binary. Each stage below is a complete deliverable with its own payoff. Stopping after any stage leaves a coherent, shippable runtime; nothing depends on a later stage landing.

Three reframings make the original wish list coherent:

- GPU execution is a kernel backend registered by the host, not a target the VM runs on.
- Scikit and pandas are dlua packages fetched with dollup, not compiled code.
- Parquet is a DRT connector, not a diluvium library.

## 2. Constraints carried forward

From `diluvium_handoff.md` and prior decisions. This spec does not reopen them.

- Core patches stay minimal, maintained as a patch series against upstream. Everything expressible through `lua.h`/`lauxlib.h` is on-top code.
- Determinism is reported, not enforced: the three-valued per-function verdict (deterministic / nondeterministic / indeterminate) plus the runtime status flag.
- Floats error when mixed with decimal (decQuad). Conversions are explicit.
- All allocation goes through diluvium's allocator so budgets and hibernation see it.
- Hostcalls are the waist. Connectors live in DRT, are feature-gated, and fail at startup by name when a program needs one the build lacks.
- Same bytecode per target. A program that uses arrays on a target built without them fails at load, by name, the same way.

## 3. Layers

| Layer | Where | Ships as |
|---|---|---|
| L0 array type, portable kernels | diluvium, on-top C behind a `numeric` feature | compiled into builds with the feature |
| L1 backend vtable | `dv.h` | tiny; always present when `numeric` is on |
| L2 libraries: dataframe, stats, ML | dlua | dollup packages |
| L3 connectors and backends: parquet, csv, wgpu | DRT (Rust) | feature-gated, native and browser as applicable |

The interpreter is not touched by any stage. Non-numeric code has identical performance before and after.

## 4. Determinism contract

Every kernel implementation carries a tier. The portable kernel defines the reference behavior.

- **exact**: integer, NTT, decQuad. Bit-identical by construction on every target.
- **reproducible**: bit-identical to the portable kernel on every target.
- **fast**: no cross-target guarantee.

Reproducible-tier requirements for the portable kernels:

1. IEEE 754 binary64, round-to-nearest-even, SSE2 baseline on x86 (no x87).
2. Compiled with `-ffp-contract=off`, no `-ffast-math`, `-fexcess-precision=standard`. FMA contraction is the most common silent divergence between targets: wasm has no FMA, x86 and ARM builds will fuse unless told not to.
3. Denormals honored (no FTZ/DAZ). GPUs commonly flush; this alone keeps most GPU kernels out of this tier.
4. No wasm relaxed-simd. Its results are platform-dependent by specification.
5. Transcendentals from the embedded libm (Stage 1), never the platform libm.
6. Reductions in canonical order (below).
7. Sorts stable, comparator total, NaN ordered last. Hashing uses a fixed 64-bit mix, never the string hash seed. Group ids assigned in first-appearance order.

Canonical reduction order (sum of n values): eight accumulators, `acc[i mod 8] += x[i]` for i ascending, tail included; combine as `((a0+a1)+(a2+a3))+((a4+a5)+(a6+a7))`. Chosen so that scalar, wasm SIMD128 (2 lanes), AVX2 (4) and AVX-512 (8) can all implement it exactly while staying vectorized. Mean is sum then one division. Variance is two-pass: canonical mean, then canonical sum of squared deviations. Dot is elementwise product (unfused) then canonical sum. GEMM computes each cell as a dot; blocking over rows and columns is free, the inner k order is not.

Verdict integration: the static analyzer classifies an array call by the tier of its portable implementation, since it cannot know the backend. The runtime flag records whether any fast-tier backend actually executed. Static says could, runtime says did, nothing is banned.

## 5. Stages

Each stage lists: deliverable, payoff if we stop here, acceptance, and the bail-out if it proves harder than expected. Dependencies are explicit; the graph is in section 6.

### Stage 0: array type and portable kernels

Depends on: nothing.

Deliverable:

- `array` userdata, dtypes `f64`, `i64`, `u8` (masks). Row-major, strided, 1D and 2D. Views (slices, transpose) share the buffer without copying. Buffers allocated through `lua_Alloc`.
- Metamethods: `__add __sub __mul __div __unm __idiv __mod __pow __len __tostring __index` (scalar get), broadcasting scalars only. Comparisons return `u8` masks via named functions; `__eq` on mixed operand types is silently false in Lua, so provide `array.eq` and friends, same note as decQuad. The float-with-decimal error applies unchanged.
- Kernels: elementwise arithmetic; reductions (`sum mean min max prod var std argmin argmax`) with optional axis; `cumsum`; `sort argsort` (stable); `where` and mask select; `dot`; `matmul` (naive, canonical order); `group_index` (key column to group ids, first-appearance order) and segmented reductions over group ids.
- Constructors: from table, `zeros ones arange linspace`, `to_table`.

Payoff if we stop here: typed arrays end the table-of-doubles memory cost, vectorize the scalar loops finance code is mostly made of, and give groupby-aggregate primitives. Useful on its own.

Acceptance:

- A corpus of array programs produces bit-identical output on native (x86-64, aarch64) and wasm32.
- Non-numeric benchmark suite shows no regression.
- Binary growth measured and recorded (guess: under 150 KB for the feature).

Bail-out: if metamethod ergonomics conflict with the float/decimal rule in ways that cannot be made explicit, ship the function API (`array.add(a, b)`) and defer operators.

### Stage 1: embedded libm and verdict extension

Depends on: nothing for libm; Stage 0 for array-tier classification.

Deliverable:

- A trimmed libm (openlibm subset: `exp log log2 log10 pow sin cos tan asin acos atan atan2 sinh cosh tanh expm1 log1p`) compiled into every build with the feature. `sqrt floor ceil fabs fmod` are already IEEE-exact and stay on the platform.
- `math.*` scalar functions routed through it at init (on-top: replace the stdlib entries). This closes the open question in the handoff doc; scalar transcendentals stop being taint sources.
- Verdict: analyzer and runtime flag extended per section 4.

Payoff if we stop here: the determinism story is complete for scalar code, independent of arrays. A contract that calls `math.exp` can carry a green verdict for the first time.

Acceptance: a test-vector set (including hard cases near branch cuts and large arguments) is bit-identical across native, wasm, and any embedded target that builds the feature. Verdict correctly downgrades when a fast-tier backend runs.

Bail-out: if openlibm does not build cleanly on a target, that target builds without the feature and `math.*` there remains a taint source. Nothing else changes.

Size guess: 50 to 150 KB, function-trimmed. Measure.

### Stage 2: FFT and NTT

Depends on: Stage 0; Stage 1 for the reproducible tier on FFT.

Deliverable:

- `c128` dtype (interleaved re/im).
- Complex FFT and inverse, real FFT (half spectrum) and inverse. Iterative radix-2, fixed algorithm and traversal, power-of-two sizes. Twiddles via embedded libm. Reproducible tier.
- NTT over a fixed prime, forward and inverse, on `i64`. Exact tier. Exact convolution and correlation built on it.
- Float convolution and correlation via FFT.

Payoff if we stop here: spectral analysis, filtering, autocorrelation and cross-correlation of return series, and an exact convolution usable near consensus.

Acceptance: FFT matches a naive DFT within tolerance on native and is bit-identical across targets; NTT convolution equals schoolbook exactly; round trips are identity for NTT and within tolerance for FFT.

Bail-out: arbitrary sizes (Bluestein) deferred; power-of-two with explicit padding is the v1 contract. If the reproducible tier fails for FFT on some target, FFT ships as fast tier there and the verdict says so.

Open decision: the prime and reduction method. A 2^64 Goldilocks-style prime needs a 128-bit intermediate, which wasm32 emulates; a multi-prime CRT over 31-bit primes avoids that at the cost of more passes.

### Stage 3: backend vtable and native CPU backend

Depends on: Stage 0.

Deliverable:

- `dv_numeric_backend` in `dv.h`: versioned struct of function pointers, one per kernel, each with a declared tier and a name. The host registers a backend before program start; unregistered kernels fall through to portable. This is the seam GPU uses later.
- First backend: native CPU. Blocked GEMM with the canonical inner order (reproducible tier); SIMD elementwise and reductions honoring section 4 (reproducible); optionally an OpenBLAS `dgemm` registered as fast tier behind its own feature.

Payoff if we stop here: real native speed on matrix work with no change to guest programs, and a proven backend seam.

Acceptance: reproducible-tier backend kernels are bit-identical to portable on the corpus; measured speedup recorded; ABI version-bump procedure documented.

Bail-out: if a SIMD kernel cannot match portable bits, it registers as fast tier. The seam is the deliverable; the speedup is the bonus.

### Stage 4: parquet and CSV connector

Depends on: Stage 0. Independent of 1, 2, 3.

Deliverable (DRT, Rust, native only):

- Connector (name TBD) scoped to a granted directory like `fs`. Verbs: read parquet (path, column selection, row range), write parquet, read CSV with dtype hints, write CSV. Built on the `parquet` crate.
- A raw-buffer lane in the hostcall encoding: a reply may carry columns as `(dtype, length, bytes)` that the guest adopts directly into `array` buffers. One copy per file, never per-element serde.
- Nulls: `f64` columns use NaN; `i64` and `u8` columns return an optional `u8` validity mask alongside.
- String columns return dictionary-encoded: `i64` codes plus a Lua table of unique strings.

Payoff if we stop here: real datasets in and out of dlua on native. This is the stage that makes Stage 0 useful on actual financial data.

Acceptance: round-trip a multi-column file with nulls and strings; a 10^7-row numeric column loads with one buffer copy (measured); a program requesting the connector on a wasm build fails at startup by name.

Bail-out: CSV first if the buffer lane takes longer than expected; parquet read-only before write.

Open decision: whether `drt-hostcall` already has a bytes payload type to extend, or the lane is new.

### Stage 5: dataframe and stats packages (dlua)

Depends on: Stage 0. Stage 4 for real-data usefulness.

Deliverable (dollup packages, names TBD):

- Dataframe: ordered columns of equal-length arrays, optional explicit index array. `select filter sort_by groupby(...):agg(...) join(how, on) rolling(window):mean|std|sum resample(bucket_key)`. Joins and groupby use `group_index` and segmented reductions from Stage 0.
- Stats: `describe corr cov ewm returns log_returns drawdown` and the usual finance summaries.

No implicit index alignment. Joins are explicit. This is the single largest source of pandas complexity and the recommendation is not to carry it.

Payoff if we stop here: a pandas-shaped workflow with zero binary growth.

Acceptance: a reference script over a parquet file produces results matching pandas on the same data (within float tolerance for fast tier, exactly for exact-tier aggregations).

Bail-out: if a needed primitive is missing from Stage 0 (a rolling kernel, a time-bucket kernel), add it there as a portable C kernel. Do not write it as a scalar dlua loop and call the stage done.

### Stage 6: linear algebra kernels and ML package

Depends on: Stage 0. Stage 3 helps speed, not correctness.

Deliverable:

- Kernels (portable C, reproducible tier): Cholesky, LU solve, symmetric eigen (cyclic Jacobi, fixed sweep order), SVD (one-sided Jacobi). Jacobi methods chosen because their sweep order is fixed and they are small.
- ML package (dlua): standard scaler, train/test split, k-fold, linear regression, ridge, logistic regression (IRLS), PCA, k-means (fixed init from the seedable RNG), kNN. Decision trees deferred.

Payoff if we stop here: scikit-shaped modeling on financial data, reproducible across targets.

Acceptance: each estimator matches scikit-learn on reference data within tolerance; bit-identical across targets.

Bail-out: if Jacobi SVD is too slow for target sizes, register a LAPACK-backed SVD as fast tier via Stage 3; the dlua package does not change.

### Stage 7: GPU backend (wgpu)

Depends on: Stage 3.

Deliverable (DRT, Rust):

- `wgpu` compute backend registered through the vtable. WGSL kernels for elementwise, reductions, GEMM first; FFT later.
- Residency: arrays gain a location; transfer is explicit (`to_device` / `to_host`) in v1. Kernels run where the data lives and error on mismatch.
- `f32` dtype added to Stage 0's enum. WGSL has no `f64`, so the wgpu path operates on `f32` arrays only and the downcast is explicit. This is a constraint of WebGPU, not a choice.
- Tier: fast, always. Denormal flushing and workgroup reduction order both violate section 4. A reproducible GPU tier is a stretch goal, not a stage.

Payoff if we stop here: batch and matrix throughput on native GPUs and in the browser via WebGPU, from one WGSL codebase.

Acceptance: results within f32 tolerance of portable; measured speedup at sizes where it matters; verdict flags execution as fast tier.

Bail-out: if wgpu proves awkward, a CUDA-only `f64` backend (native, NVIDIA) through the same vtable is the fallback, giving up the browser.

## 6. Dependency graph

```
Stage 0
  |-- Stage 1  (enables reproducible-tier FFT in Stage 2)
  |-- Stage 2
  |-- Stage 3 --> Stage 7
  |-- Stage 4  (makes Stage 5 useful on real data)
  |-- Stage 5
  '-- Stage 6
```

Stages 1 through 4 are mutually independent. Suggested order: 0, 1, 4, 5, 2, 3, 6, 7. That gets real data flowing (4, 5) before the harder numerics, and puts GPU last because it has the least certain payoff.

## 7. Cross-platform matrix (intended)

| Target | Stages | Backends |
|---|---|---|
| native x86-64 / aarch64 | 0 to 7 | portable, CPU SIMD, optional BLAS, wgpu |
| wasm32 (WASI, unknown) | 0, 1, 2, 5, 6 | portable (wasm SIMD128 allowed, relaxed-simd not) |
| browser | 0, 1, 2, 5, 6, 7 | portable, wgpu via WebGPU (f32) |
| RP2350 class | feature off by default | none |

## 8. Size and performance budget

- Interpreter untouched. Acceptance for every stage includes no regression on the non-numeric benchmark suite.
- Core growth is Stages 0, 1, 2, 6 kernels only. Rough guess for all four combined: under 600 KB, feature-gated. Parquet, CPU SIMD, BLAS and wgpu add nothing to diluvium.
- Every stage records measured binary growth in its acceptance report; the guesses above are replaced by numbers as stages land.

## 9. Open decisions (deliberately not made here)

- Names: the array type, the connector, the dollup packages.
- NTT prime and reduction strategy (Stage 2).
- Hostcall raw-buffer lane: extend or new (Stage 4).
- Whether `u8` masks are the permanent bool representation or a `bool` dtype is warranted.
- Whether a time dtype (`i64` epoch nanos with a unit tag) is worth adding for resample, or a convention suffices.

## 10. Non-goals

- Running the diluvium VM on a GPU. Revisit only if a same-bytecode, many-independent-states, compute-dense workload appears; nothing in Vera, discofetch, or the analytics use case has that shape.
- A pure-C parquet reader.
- pandas index alignment semantics.
- Reproducible-tier GPU kernels (stretch, not a stage).
- Arbitrary-size FFT in v1.
- Decimal arrays. decQuad stays scalar and lives at the boundary; analytics converts to `f64` explicitly and back under a stated rounding mode.
