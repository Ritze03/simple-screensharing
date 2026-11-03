#!/usr/bin/env python3
import re, sys
# packets per second the in-process viewer received, from the soak test log
for path in sys.argv[1:]:
    prev = None; out = []
    for line in open(path):
        m = re.search(r"\[soak\] t=\s*(\d+)s .*viewer packets (\d+)", line)
        if m:
            p = int(m.group(2))
            if prev is not None: out.append(p - prev)
            prev = p
    print(path.split('/')[-2], ' '.join(map(str, out)))
