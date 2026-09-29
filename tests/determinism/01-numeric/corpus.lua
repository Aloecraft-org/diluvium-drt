-- test/numeric/corpus.lua
-- The cross-target corpus (doc/Plan-2026-09.md A2 and 3.3).
--
-- Every line this prints is an IEEE bit pattern or an integer, never a
-- decimal: glibc, musl, wasmtime's host, Chromium and mingw do not all
-- format a double the same way, and one expected.txt has to be valid on
-- all of them. So the diff this file feeds is a diff of bits, and a
-- single differing digit is a real divergence rather than a formatting
-- one.
--
-- Run through script/numeric_corpus.sh, which diffs the output against
-- test/numeric/expected.txt.

local function say(name, s) print(name .. ": " .. s) end
local function bits(a) return array.bits(a) end

-- One scalar as bits, through the same route an example would use, so
-- the reduction results below can be read the same way.
local function fbits(x)
  return ("%016x"):format(string.unpack("<I8", string.pack("<d", x)))
end

-- 1. Construction and the element types.
local f = array.from{1.0, 2.5, -3.75, 0.1, 1e300, 1e-300}
-- 'math.mininteger', not the literal: -9223372036854775808 is a unary
-- minus applied to a numeral that does not fit an integer, so Lua reads
-- it as a float and the whole table would become f64.
local i = array.from{1, -1, 9007199254740993, math.mininteger, math.maxinteger}
local u = array.cast(array.from{0, 1, 255, 256}, "u8")
say("f64", bits(f))
say("i64", bits(i))
say("u8", bits(u))
say("arange_i", bits(array.arange("i64", -3, 4, 2)))
say("arange_f", bits(array.arange("f64", 0.0, 1.0, 0.25)))
say("linspace", bits(array.linspace(-1.0, 1.0, 9)))
say("zeros", bits(array.zeros("f64", 3)))
say("ones", bits(array.ones("f64", 3)))

-- 2. Elementwise, including the operators that are float by definition.
local a = array.linspace(0.1, 0.9, 8)
local b = array.linspace(0.9, 0.1, 8)
say("add", bits(a + b))
say("sub", bits(a - b))
say("mul", bits(a * b))
say("div", bits(a / b))
say("pow", bits(a ^ b))
say("mod", bits(array.mod(a, b)))
say("idiv", bits(array.idiv(a, b)))
say("neg", bits(-a))
say("scalar", bits(a * 3.0))
say("int_add", bits(array.from{1, 2, 3} + array.from{10, 20, 30}))
say("int_idiv", bits(array.idiv(array.from{-7, 7, -7, 7}, array.from{2, 2, -2, -2})))
say("int_mod", bits(array.mod(array.from{-7, 7, -7, 7}, array.from{2, 2, -2, -2})))

-- 3. The canonical reduction order. The magnitudes are chosen so that a
--    different accumulator order gives a different answer: a naive
--    left-to-right sum absorbs the ones into the large first element and
--    this does not.
local hard = {1e16}
for k = 1, 24 do hard[#hard + 1] = 1.0 end
local h = array.from(hard)
say("sum_hard", fbits(array.sum(h)))
say("mean_hard", fbits(array.mean(h)))
say("sum", fbits(array.sum(a)))
say("mean", fbits(array.mean(a)))
say("var", fbits(array.var(a)))
say("std", fbits(array.std(a)))
say("prod", fbits(array.prod(a)))
say("min", fbits(array.min(a)))
say("max", fbits(array.max(a)))
say("argmin", tostring(array.argmin(a)))
say("argmax", tostring(array.argmax(a)))
say("cumsum", bits(array.cumsum(h)))
say("int_sum", tostring(array.sum(array.from{1, 2, 3, -6})))
say("int_prod", tostring(array.prod(array.from{2, 3, 7})))

-- 4. dot and matmul: the products are formed unfused and then summed in
--    the canonical order, which is exactly where a fused multiply-add
--    would show up as a different last digit.
say("dot", fbits(array.dot(a, b)))
say("dot_hard", fbits(array.dot(h, array.ones("f64", #h))))
local m = array.from{{1.5, 2.5, 3.5}, {0.25, 0.5, 0.75}}
local n = array.transpose(m)
say("matmul", bits(array.matmul(m, n)))
say("matmul_t", bits(array.matmul(n, m)))

-- 5. Ordering: stable, total, NaN last.
--
-- The NaN is built from its bits rather than computed. IEEE 754 does not
-- interpret the sign of a NaN and does not say which one an invalid
-- operation produces: x86-64's '0.0/0.0' is fff8000000000000 and
-- aarch64's is 7ff8000000000000, and both are correct. 'sort' moves
-- values without touching them, so a computed NaN would put that choice
-- into this file's output and report a divergence that is not one.
--
-- What is worth pinning is pinned: the ordering, and that a NaN carried
-- through 'sort' comes out with the bits it went in with.
local nan = string.unpack("<d", string.pack("<I8", 0x7ff8000000000000))
local mixed = array.from{3.0, nan, 1.0, -0.0, 0.0, nan, 2.0, -1.0}
say("sort", bits(array.sort(mixed)))
say("argsort", bits(array.argsort(mixed)))
say("argsort_ties", bits(array.argsort(array.from{2, 1, 2, 1, 2, 1})))
-- The property the line above can no longer state: whatever NaN this
-- target's FPU makes, it still sorts last. Reported as where it landed
-- rather than as what it is, which is the part that is portable.
local fpu = array.sort(array.from{1.0, 0.0 / 0.0, -1.0})
local last = array.get(fpu, 3)
say("sort_computed_nan_last", tostring(last ~= last))

-- 6. Masks and selection.
local mask = array.gt(a, 0.5)
say("gt", bits(mask))
say("eq", bits(array.eq(array.from{1, 2, 3}, array.from{1, 5, 3})))
say("select", bits(array.select(a, mask)))
say("where", bits(array.where(mask, a, -1.0)))

-- 7. Grouping: ids in first-appearance order, and the segmented
--    reductions that hang off them.
local keys = array.from{7, 3, 7, 9, 3, 7}
local ids, ng = array.group_index(keys)
say("group_ids", bits(ids))
say("group_n", tostring(ng))
local vals = array.from{1.0, 2.0, 4.0, 8.0, 16.0, 32.0}
say("group_sum", bits(array.group_sum(vals, ids, ng)))
say("group_mean", bits(array.group_mean(vals, ids, ng)))
say("group_count", bits(array.group_count(vals, ids, ng)))
say("group_min", bits(array.group_min(vals, ids, ng)))
say("group_max", bits(array.group_max(vals, ids, ng)))
-- -0.0 and +0.0 are one key, and every NaN is one key.
local gk, gn = array.group_index(array.from{0.0, -0.0, nan, nan, 1.0})
say("group_zero_nan", bits(gk) .. " n=" .. gn)

-- 8. Views: a slice, a row and a transpose read the same elements the
--    copies do, so a kernel over a view is the kernel over the copy.
local big = array.from{{1.0, 2.0, 3.0, 4.0}, {5.0, 6.0, 7.0, 8.0}}
say("row", bits(array.row(big, 2)))
say("slice", bits(array.slice(array.linspace(0.0, 1.0, 5), 2, 4)))
say("transpose", bits(array.copy(array.transpose(big))))
say("view_sum", fbits(array.sum(array.transpose(big))))
say("axis1", bits(array.sum(big, 1)))
say("axis2", bits(array.sum(big, 2)))

-- 9. The embedded libm (stage 1).
--
-- These are the reason it exists. glibc, musl, Apple's, wasi-libc's and
-- mingw's are five correct implementations that disagree in the last bit,
-- so a program whose answer depends on one has no cross-target answer;
-- routed through the vendored openlibm they all give what is below.
--
-- The values are not arbitrary. 'exp_glibc_differs' is a point where this
-- build and glibc disagree in the last bit -- measured, not guessed -- so
-- a build where the routing failed to install fails this line rather than
-- passing quietly.
local function f1(name, fn, x) say(name, fbits(fn(x))) end
f1("exp", math.exp, 1.0)
f1("exp_neg", math.exp, -1.0)
f1("exp_glibc_differs", math.exp, -20.505154639175259)
-- The '^' operator is OP_POW, not a 'math' entry, and reaches the vendored
-- pow through luaconf.h's luai_numpow rather than through the install over
-- 'math'. Same idea as the line above: on glibc the platform's pow answers
-- ...575 here, the vendored one ...574, so a build where the operator
-- slipped back to the platform fails this line on Linux and on nothing
-- else -- which is the cross-target diff doing its job.
say("pow_op_glibc_differs", fbits(1.3436666353689972 ^ -7.278736595197527))
f1("exp_large", math.exp, 709.0)
f1("exp_small", math.exp, -745.0)
f1("log", math.log, 2.0)
f1("log_near1", math.log, 1.0000000000000002)
f1("log10", function(x) return math.log(x, 10) end, 7.0)
f1("log2", function(x) return math.log(x, 2) end, 7.0)
say("log_exact", tostring(math.log(8, 2)) .. " " .. tostring(math.log(100, 10)))
f1("sin", math.sin, 1.0)
f1("cos", math.cos, 1.0)
f1("tan", math.tan, 1.0)
-- Argument reduction is where two libms diverge most, so the large ones
-- are the interesting cases rather than a formality.
f1("sin_huge", math.sin, 1e22)
f1("cos_huge", math.cos, 1e300)
f1("tan_huge", math.tan, 1e22)
f1("sin_pi", math.sin, 3.141592653589793)
f1("asin", math.asin, 0.5)
f1("acos", math.acos, 0.5)
f1("atan", math.atan, 1.0)
say("atan2", fbits(math.atan(1.0, -1.0)))
-- Exact by IEEE 754 and therefore still the platform's: these lines say
-- so, and would change if one were ever routed by mistake.
f1("sqrt", math.sqrt, 2.0)
f1("floor", math.floor, -1.5)
f1("fmod", function(x) return math.fmod(x, 2.0) end, 5.5)

-- 8. Stage 2: the transforms. A transform's whole claim is that its bits
--    are the same on every target, and these lines are where that is
--    checked rather than asserted. The FFT sizes are small on purpose:
--    the divergence a target introduces is in the twiddle, and it shows
--    at n = 8 as clearly as at n = 8192.
local sig = array.from{1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0}
say("fft", bits(array.fft(sig)))
say("ifft", bits(array.ifft(array.fft(sig))))
say("rfft", bits(array.rfft(sig)))
say("irfft", bits(array.irfft(array.rfft(sig))))
-- A signal with no symmetry to hide a wrong twiddle behind.
local wob = {}
for k = 1, 16 do wob[k] = math.sin(k * 1.7) * 3.0 + k * 0.25 end
say("fft16", bits(array.fft(array.from(wob))))
say("fft16_mag", bits(array.magnitude(array.fft(array.from(wob)))))
-- Complex in, complex out, so the input's imaginary part is exercised.
say("fft_c", bits(array.fft(array.complex(array.from{1.0, -2.0, 3.0, -4.0},
                                          array.from{0.5, 0.25, -0.5, -0.25}))))
say("conv_f", bits(array.convolve(array.from{1.5, -2.5, 3.5},
                                  array.from{0.5, 0.25})))
-- The NTT is integer arithmetic and cannot drift; these lines say so,
-- and would catch a prime or a root that changed by accident.
local iv = array.from{5, 7, 11, 13, 17, 19, 23, 29}
say("ntt", bits(array.ntt(iv)))
say("intt", bits(array.intt(array.ntt(iv))))
say("conv_i", bits(array.convolve(array.from{1, -2, 3, 4, 5},
                                  array.from{7, 8, -9})))
say("corr_i", bits(array.correlate(array.from{1, 2, 3},
                                   array.from{0, 1, 0})))
