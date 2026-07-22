#!/usr/bin/env python3
"""CI policy for security-sensitive kernel modules.

Legacy unsafe sites are handled incrementally; this gate prevents undocumented
unsafe from entering the newly hardened trust-boundary modules.
"""
from pathlib import Path
import re, sys
root = Path(__file__).resolve().parents[1]
critical = [
    root/'src/security',
    root/'src/arch/x86_64/linux/mod.rs',
    root/'src/memory/vmm.rs',
    root/'src/net/tls.rs',
    root/'src/pkg/apt.rs',
]
files = []
for item in critical:
    files.extend(item.rglob('*.rs') if item.is_dir() else [item])
errors = []
for path in files:
    lines = path.read_text(encoding='utf-8').splitlines()
    for i, line in enumerate(lines):
        if re.search(r'\bunsafe\s*\{', line):
            window = '\n'.join(lines[max(0, i-6):i+1])
            if 'SAFETY:' not in window:
                errors.append(f'{path.relative_to(root)}:{i+1}: unsafe block lacks nearby SAFETY comment')
if errors:
    print('\n'.join(errors)); sys.exit(1)
print(f'safety policy: OK ({len(files)} critical files)')
