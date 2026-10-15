import io
import sys

fs, fe, vs, ve = [int(x) for x in open('.agent-scratch/m2_ranges.txt').read().split()]
lines = open('src/io/uring.rs').read().split('\n')

fused = lines[fs:fe + 1]
vec = lines[vs:ve + 1]

assert fused[0].strip() == 'let completion_result = match pending_op {', fused[0]
assert vec[0].strip() == 'let completion_result = match pending_op {', vec[0]

# The vec copy carries the SAFETY comments the fused copy lost; use it as the
# canonical body and normalize its patterns to the `..` form the fused copy used.
canon = vec[:]
canon[0] = '    match pending_op {'

# Normalize `op: _op,` in destructuring patterns to `..`. Only patterns that
# already bind other fields need this; single-field patterns become `{ op: .. }`
# instead of `{ op: _op }`.
canon = [l.replace('op: _op,\n', '') for l in canon]
canon = [l for l in canon if l.strip() != 'op: _op,']
canon = [l.replace('op: _op }', 'op: .. }') for l in canon]
canon = [l.replace('{ op: _op,', '{ op: ..,') for l in canon]
canon = [l.replace(', op: _op,\n', ',\n') for l in canon]

open('.agent-scratch/canon_body.txt', 'w').write('\n'.join(canon))
print('\n'.join(canon[:40]))
print('...')
print("total lines:", len(canon))
