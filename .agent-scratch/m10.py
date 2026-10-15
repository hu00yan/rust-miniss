files = ['src/cancellation.rs', 'src/net.rs', 'src/quic.rs', 'src/timer/sleep.rs']
n = 0
for p in files:
    s = open(p).read()
    s2 = s.replace(
        "let this = unsafe { self.get_unchecked_mut() };",
        "let this = self.get_mut();",
    )
    s2 = s2.replace(
        "let this = unsafe { self.as_mut().get_unchecked_mut() };",
        "let this = self.as_mut().get_mut();",
    )
    if s2 != s:
        n += 1
        open(p, 'w').write(s2)
print("files touched:", n)
