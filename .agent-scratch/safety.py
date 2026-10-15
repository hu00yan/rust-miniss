p = 'src/io/uring.rs'
s = open(p).read()

SAFETY = (
    "                // SAFETY: `pending_op` came from this slot and its CQE is the one\n"
    "                // being consumed right now, so every kernel-written region it owns is\n"
    "                // valid for exactly `result` bytes. See `reconstruct_completion` for\n"
    "                // the full contract.\n"
    "                // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.\n"
)

# 1. Vec call site (deeper indentation, 16 spaces)
old_vec = (
    "                let completion_result = unsafe { Self::reconstruct_completion(pending_op, result) };\n"
    "                completions.push((token, completion_result));"
)
new_vec = (
    "                // Reconstruct the terminal completion from the slot's pending\n"
    "                // operation and this CQE's result.\n"
    + SAFETY.replace("                ", "                ", 1) +
    "                let completion_result =\n"
    "                    unsafe { Self::reconstruct_completion(pending_op, result) };\n"
    "                completions.push((token, completion_result));"
)
assert old_vec in s, "vec call site not found"
s = s.replace(old_vec, new_vec, 1)

# 2. Fused call site
old2 = (
    "                let completion_result = unsafe { Self::reconstruct_completion(pending_op, result) };"
)
assert old2 in s, "fused call site not found"
new2 = (
    "                // Reconstruct the terminal completion from the slot's pending\n"
    "                // operation and this CQE's result.\n"
    + SAFETY +
    "                let completion_result =\n"
    "                    unsafe { Self::reconstruct_completion(pending_op, result) };"
)
s = s.replace(old2, new2, 1)

open(p, 'w').write(s)
print("ok")
