"""Lists the menu layout table the game builds `menu:/<name>.sblytbnd` paths from.

Usage:
    run.ps1 menu_layouts [<table rva>] [<entries>]

The table is an array of 16-byte entries: a pointer to a UTF-16 name and a flags word whose
byte 1 says the file sits under `Hi/`. Writes menu_layouts.txt: `index name hi`.
"""
import os
import sys

import idc

sys.path.insert(0, os.path.dirname(os.path.abspath(idc.ARGV[0])))

import ida_bytes
import idaapi

import common


def read_wstr(ea):
    out = []
    while len(out) < 80:
        ch = ida_bytes.get_word(ea + 2 * len(out))
        if ch == 0:
            break
        out.append(chr(ch))
    return "".join(out)


def main():
    common.init()
    pos, _ = common.parse_args()
    table = common.BASE + int(pos[0], 16) if pos else common.BASE + 0x2BC08F0
    count = int(pos[1]) if len(pos) > 1 else 64
    lines = []
    for i in range(count):
        entry = table + 16 * i
        ptr = ida_bytes.get_qword(entry)
        if not ptr or ptr < common.BASE or ptr > common.BASE + 0x6000000:
            lines.append(f"{i}\t<{ptr:#x}>")
            continue
        lines.append(f"{i}\t{read_wstr(ptr)}\thi={ida_bytes.get_byte(entry + 9)}")
    path = os.path.join(common.OUT_DIR, "menu_layouts.txt")
    with open(path, "w", encoding="utf-8") as f:
        f.write("\n".join(lines) + "\n")
    common.finish()


main()
