#!/usr/bin/env python3
"""Run host tests on the actual host triple, overriding kernel Cargo config."""
import subprocess
host = next(line.split(': ', 1)[1] for line in subprocess.check_output(['rustc', '-vV'], text=True).splitlines() if line.startswith('host: '))
subprocess.run(['cargo', 'test', '--locked', '--target', host], cwd='host-tests', check=True)
