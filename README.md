# Lazy HTTPS Filesystem

This is a FUSE filesystem used to acess links as if they were
files. This is intended to shrink the size of CI/CD docker images
by lazily loading infrequently used assets.
