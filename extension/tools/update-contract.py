#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-or-later
"""Writes extension/contract.js from docs/io.github.d101c.Pods1.xml, or with
--check, fails when contract.js is not that file word for word.

The extension carries the D-Bus contract as a JS string so that the Shell
reads no file when the extension starts.

    extension/tools/update-contract.py           rewrite contract.js
    extension/tools/update-contract.py --check   (scripts/pack.sh runs this)
"""
import pathlib
import sys

EXT = pathlib.Path(__file__).resolve().parents[1]
XML = EXT.parent / 'docs' / 'io.github.d101c.Pods1.xml'
JS = EXT / 'contract.js'

HEAD = '''// SPDX-License-Identifier: GPL-2.0-or-later
// The D-Bus contract between the engine and the extension:
// docs/io.github.d101c.Pods1.xml word for word, written by
// tools/update-contract.py (scripts/pack.sh checks it). Do not edit by hand.

export default `'''
TAIL = '`;\n'


def render():
    xml = XML.read_text()
    if '`' in xml or '${' in xml or '\\' in xml:
        sys.exit('the contract holds characters a JS template string would change')
    return HEAD + xml + TAIL


def main():
    wanted = render()
    if '--check' in sys.argv[1:]:
        if not JS.exists() or JS.read_text() != wanted:
            sys.exit('extension/contract.js differs from docs/io.github.d101c.Pods1.xml: '
                     'run extension/tools/update-contract.py')
        return
    JS.write_text(wanted)
    print(JS)


if __name__ == '__main__':
    main()
