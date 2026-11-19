#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-or-later
"""Prints an engine-release.json for one archive of the headless test:

    release-json.py ARCHIVE URL > engine-release.json

version 9.9.9, this machine's architecture, the archive's size and SHA-256.
"""
import hashlib
import json
import os
import sys

archive, url = sys.argv[1:]
data = open(archive, 'rb').read()
asset = {'url': url, 'size': len(data), 'sha256': hashlib.sha256(data).hexdigest()}
print(json.dumps({'version': '9.9.9', 'assets': {os.uname().machine: asset}}, indent=2))
