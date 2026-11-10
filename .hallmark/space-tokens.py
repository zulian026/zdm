#!/usr/bin/env python3
"""Replace raw spacing lengths in padding/margin/gap declarations with
Hallmark --space-* tokens (sloplint gate 24a). Values with no matching
token (20px, 28px, 48px, ...) are left raw: they sit on the 4px scale,
so gate 24b does not flag them. Comments are never touched.
Usage: python3 space-tokens.py src/styles.css"""
import re
import sys

SPACE = {
    2: "--space-3xs",
    4: "--space-2xs",
    8: "--space-xs",
    12: "--space-sm",
    16: "--space-md",
    24: "--space-lg",
    40: "--space-xl",
    64: "--space-2xl",
    96: "--space-3xl",
    144: "--space-4xl",
}

PROP = re.compile(
    r"((?:padding|margin|gap|row-gap|column-gap)(?:-[a-z]+)?\s*:\s*)([^;{}]+?)(;)"
)
NUM = re.compile(r"(?<![\w.-])(-?)(\d+(?:\.\d+)?)px\b")


def replace_nums(value: str) -> str:
    def sub(m: re.Match) -> str:
        neg, num = m.group(1), float(m.group(2))
        token = SPACE.get(int(num)) if num == int(num) else None
        if token is None:
            return m.group(0)
        if neg:
            return f"calc(var({token}) * -1)"
        return f"var({token})"

    return NUM.sub(sub, value)


def process_code(code: str) -> str:
    return PROP.sub(lambda m: m.group(1) + replace_nums(m.group(2)) + m.group(3), code)


def main(path: str) -> None:
    src = open(path, encoding="utf-8").read()
    parts = re.split(r"(/\*.*?\*/)", src, flags=re.S)
    out = "".join(p if p.startswith("/*") else process_code(p) for p in parts)
    if out != src:
        open(path, "w", encoding="utf-8").write(out)
        print(f"{path}: spacing tokens applied")
    else:
        print(f"{path}: no change")


if __name__ == "__main__":
    main(sys.argv[1])
