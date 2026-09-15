# SPDX-License-Identifier: BSD-2-Clause
"""Gross pixels: per-channel difference over 48, the §9.1 metric.

Not a percentage of a percentage and not a mean: a mean hides a hundred wrong pixels in a
million right ones, and the thing worth knowing is how many pixels a person would call
different. 48 is the threshold that metric is defined at; 12 is the second lens, for when
a change should move nothing at all.
"""

import sys

from PIL import Image

threshold = int(sys.argv[3]) if len(sys.argv) > 3 else 48
a = Image.open(sys.argv[1]).convert("RGB").tobytes()
b = Image.open(sys.argv[2]).convert("RGB").tobytes()
if len(a) != len(b):
    sys.exit(f"different sizes: {len(a) // 3} against {len(b) // 3} pixels")
n = sum(
    1
    for i in range(0, len(a), 3)
    if max(abs(a[i] - b[i]), abs(a[i + 1] - b[i + 1]), abs(a[i + 2] - b[i + 2])) > threshold
)
total = len(a) // 3
print(f"gross {n} of {total}  ({100.0 * n / total:.3f}%)")
