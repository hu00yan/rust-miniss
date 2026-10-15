fs, fe, vs, ve = [int(x) for x in open('.agent-scratch/m2_ranges.txt').read().split()]
src = open('src/io/uring.rs').read()
lines = src.split('\n')
canon = open('.agent-scratch/canon_body.txt').read().split('\n')

# --- 1. Build the extracted associated function -------------------------------
FN = '''/// Reconstruct the terminal completion for a slot whose operation just
/// finished.
///
/// Both drain paths need this, and both used to carry their own copy of the
/// `match`: the fused drain (`drain_completions_fused_inline`) and the Vec drain
/// (`drain_completions_into`). They were written independently and drifted —
/// the fused copy lost every SAFETY comment, renamed the sockaddr locals, and
/// bound patterns differently. A terminal-completion bug therefore had to be
/// found and fixed twice, and a fix that missed one copy was silently Linux-only
/// (the Vector path) or fused-only. Nothing in the logic actually depends on
/// which drain is running, so there is exactly one copy now.
///
/// This is `unsafe` for the same reason the two copies were: it publishes
/// kernel-written bytes as initialized Rust memory (`Buffer::set_len`) and
/// reads kernel-filled `sockaddr` memory. Both are valid only for the CQE being
/// consumed.
///
/// # Safety
///
/// - `pending_op` must have just reached terminal completion for the CQE whose
///   `result` is `result`. Every kernel-written region it owns must be valid for
///   exactly `result` bytes — no more.
/// - For `UdpRecv`/`UdpSend`/`Connect`/`Accept`, the `addr_storage` box must
///   still hold the `sockaddr` bytes the kernel filled for *this* completion,
///   and `addr_len` the length it reported.
/// - Not safe against a malicious or corrupted CQE length: `result` is trusted
///   as an upper bound on the kernel's write (see
///   `docs/review/runtime-safety-contracts.md` C8).
unsafe fn reconstruct_completion(
    pending_op: PendingOp,
    result: i32,
) -> Result<CompletionKind, IoError> {
    if result < 0 {
        return Err(IoError::Io(io::Error::from_raw_os_error(-result)));
    }
'''

# Strip the per-arm `if result < 0 { Err(..) } else {` wrapping? No: several arms
# legitimately handle their own error paths (partial writes, EOF). Keep the arms
# verbatim from the canonical body and leave the shared negative-result check
# out so error semantics stay exactly as they were in both copies.
FN = FN.replace(
    '''    if result < 0 {
        return Err(IoError::Io(io::Error::from_raw_os_error(-result)));
    }
''',
    '',
)

FN = fn_body = FN + '\n'.join(canon) + '\n}\n'

# --- 2. Replace both copies with a call --------------------------------------
old_src = src

# Replace the Vec copy first (later offset) so the fused offsets stay valid.
vec_block = '\n'.join(lines[vs:ve + 1])
assert vec_block in old_src, "vec block not found verbatim"

vec_call = '''                let completion_result = unsafe { Self::reconstruct_completion(
                    pending_op,
                    result,
                ) };'''

new_src = old_src.replace(vec_block, vec_call, 1)

# Re-locate the fused block in the updated source (offsets shifted).
lines2 = new_src.split('\n')
fused_block = '\n'.join(lines[fs:fe + 1])
assert fused_block in new_src, "fused block not found verbatim after first replace"

fused_call = '''                let completion_result = unsafe {
                    Self::reconstruct_completion(pending_op, result)
                };'''
new_src = new_src.replace(fused_block, fused_call, 1)

# --- 3. Insert the function before `impl IoProvider for UringBackend` ----------
anchor = 'impl IoProvider for UringBackend {'
assert new_src.count(anchor) == 1, new_src.count(anchor)
new_src = new_src.replace(anchor, FN + '\n' + anchor, 1)

open('src/io/uring.rs', 'w').write(new_src)
print("removed:", len(lines[fs:fe + 1]) + len(lines[vs:ve + 1]), "lines of duplicated match")
print("added:", FN.count('\n'), "lines of shared function")
