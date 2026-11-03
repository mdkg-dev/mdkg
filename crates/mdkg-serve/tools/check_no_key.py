#!/usr/bin/env python3
"""Re-derive and check enroll.rs's NO_KEY (Opus #7228).

NO_KEY is SHA-256 of b"mdkg-serve no-key stand-in <n>" for the first n
whose 32 bytes decode to a canonical Ed25519 point of the prime-order
subgroup: not small order (8*P is not the identity) and L*P is the
identity. Pure Python, the curve's own arithmetic (RFC 8032), no
dependencies. First checks the code on the base point B.

    python3 -I crates/mdkg-serve/tools/check_no_key.py
prints the counter and the key, and exits non-zero if anything fails.
"""
import hashlib
import pathlib
import re
import sys

p = 2**255 - 19
L = 2**252 + 27742317777372353535851937790883648493
d = (-121665 * pow(121666, p - 2, p)) % p
SQRT_M1 = pow(2, (p - 1) // 4, p)
IDENTITY = (0, 1)
ENROLL_RS = pathlib.Path(__file__).resolve().parent.parent / "src" / "enroll.rs"


def no_key_in_enroll_rs():
    """The NO_KEY bytes as enroll.rs defines them (#7897, #7954)."""
    text = ENROLL_RS.read_text()
    m = re.search(r"const NO_KEY: \[u8; 32\] = \[(.*?)\];", text, re.S)
    if m is None:
        sys.exit("NO_KEY not found in " + str(ENROLL_RS))
    # Every element must be a two-digit hex literal and nothing else, so
    # an expression such as `0x36 + 1` can't be misread (Terra 7 #8157).
    items = [i.strip() for i in m.group(1).split(",")]
    if items and items[-1] == "":
        items.pop()
    if len(items) != 32 or not all(re.fullmatch(r"0x[0-9a-fA-F]{2}", i) for i in items):
        sys.exit("NO_KEY in enroll.rs must be 32 hex literals and nothing else, no comments")
    return bytes(int(i, 16) for i in items)


def recover_x(y, sign):
    xx = (y * y - 1) * pow(d * y * y + 1, p - 2, p) % p
    x = pow(xx, (p + 3) // 8, p)
    if (x * x - xx) % p:
        x = x * SQRT_M1 % p
    if (x * x - xx) % p:
        return None
    if x != 0 and x & 1 != sign:
        x = p - x
    return x


def add(a, b):
    (x1, y1), (x2, y2) = a, b
    t = d * x1 * x2 * y1 * y2
    return ((x1 * y2 + x2 * y1) * pow(1 + t, p - 2, p) % p,
            (y1 * y2 + x1 * x2) * pow(1 - t, p - 2, p) % p)


def mul(n, pt):
    r = IDENTITY
    while n:
        if n & 1:
            r = add(r, pt)
        pt = add(pt, pt)
        n >>= 1
    return r


def decode(enc):
    sign = enc[31] >> 7
    y = int.from_bytes(bytes(enc[:31]) + bytes([enc[31] & 0x7F]), "little")
    if y >= p:
        return None
    x = recover_x(y, 0)
    if x is None or (x == 0 and sign == 1):
        # x = 0 with the sign bit set is a non-canonical encoding (#7897).
        return None
    if x & 1 != sign:
        x = p - x
    return (x, y)


def full_order(pt):
    return mul(8, pt) != IDENTITY and mul(L, pt) == IDENTITY


def main():
    base = decode(bytes.fromhex("58" + "66" * 31))
    if base is None or not full_order(base):
        sys.exit("the curve code fails on the base point")
    # x = 0 with the sign bit set (the identity, non-canonically) doesn't
    # decode at all (#7897, Opus #8139, Terra 7 #8157).
    if decode(bytes.fromhex("01" + "00" * 30 + "80")) is not None:
        sys.exit("the decoder accepts a non-canonical encoding")
    # Small-order encodings are refused (#7897).
    for bad in ["00" * 31 + "80", "ec" + "ff" * 30 + "7f"]:
        pt = decode(bytes.fromhex(bad))
        if pt is not None and full_order(pt):
            sys.exit("the decoder accepts " + bad)
    expected = no_key_in_enroll_rs().hex()
    for n in range(1000):
        h = hashlib.sha256(b"mdkg-serve no-key stand-in %d" % n).digest()
        pt = decode(h)
        if pt is not None and full_order(pt):
            print(n, h.hex())
            if h.hex() != expected:
                sys.exit("NO_KEY in enroll.rs no longer matches the derivation")
            return
    sys.exit("no counter below 1000 gives a full-order point")


if __name__ == "__main__":
    main()
