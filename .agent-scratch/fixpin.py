p = 'src/cancellation.rs'
s = open(p).read()
s = s.replace("use pin_project_lite::pin_project;", "use pin_project::pin_project;", 1)
open(p, 'w').write(s)
