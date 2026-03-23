"""
Given a comma (e.g. 169:168), print all its least-degree
harmonic equations, ignoring powers of 2.

A comma n:d factors into primes on each side.
Ignoring 2, we get a "simplest equation" like 13^2 = 3*7.

Each prime has a fixed "degree" (its total exponent).
We enumerate all equations derived by multiplying or dividing
both sides by primes. Each prime (with its full degree) ends up
on either the LHS or RHS. Fractions result when a prime moves
to the opposite side from where it started.

Usage:
    python comma_equations.py 169:168
    python comma_equations.py 81:80
"""

import sys
from itertools import product
from sympy import factorint


def comma_to_prime_degrees(n, d):
    """Factor n:d, strip powers of 2.
    Return {prime: (degree, original_side)}
    where original_side is 'L' (from n) or 'R' (from d)."""
    nf = factorint(n)
    df = factorint(d)
    nf.pop(2, None)
    df.pop(2, None)
    primes = sorted(set(nf) | set(df))
    result = {}
    for p in primes:
        lhs = nf.get(p, 0)
        rhs = df.get(p, 0)
        common = min(lhs, rhs)
        lhs -= common
        rhs -= common
        if lhs > 0:
            result[p] = (lhs, 'L')
        elif rhs > 0:
            result[p] = (rhs, 'R')
    return result


def format_side(num_parts, den_parts):
    """Format one side of the equation as 'A * B / (C * D)'.
    num_parts and den_parts are lists of (prime, exponent)."""
    def fmt(parts):
        if not parts:
            return "1"
        terms = []
        for p, e in sorted(parts):
            terms.append(str(p) if e == 1 else f"{p}^{e}")
        return " * ".join(terms)
    n = fmt(num_parts)
    d = fmt(den_parts)
    if not den_parts:
        return n
    if not num_parts:
        return f"1 / ({d})" if len(den_parts) > 1 else f"1 / {d}"
    if len(den_parts) > 1:
        return f"{n} / ({d})"
    return f"{n} / {d}"


def all_least_degree_equations(prime_info):
    """Each prime p with degree d can be split across sides.
    Choose k in {0, ..., d}: k copies stay on p's original side
    (in the numerator), d-k copies move to the other side
    (in the denominator, since they crossed the = sign).
    """
    primes = sorted(prime_info.keys())
    # For each prime, the choices are 0..degree (how many stay on original side)
    choices = [range(prime_info[p][0] + 1) for p in primes]

    results = []
    for combo in product(*choices):
        lhs_num = []
        lhs_den = []
        rhs_num = []
        rhs_den = []
        for p, k in zip(primes, combo):
            deg, orig = prime_info[p]
            stay = k       # copies on original side (numerator)
            move = deg - k # copies on other side (denominator)
            if orig == 'L':
                if stay > 0: lhs_num.append((p, stay))
                if move > 0: rhs_den.append((p, move))
            else:
                if stay > 0: rhs_num.append((p, stay))
                if move > 0: lhs_den.append((p, move))
        lhs = (tuple(sorted(lhs_num)), tuple(sorted(lhs_den)))
        rhs = (tuple(sorted(rhs_num)), tuple(sorted(rhs_den)))
        # Normalize: swap sides to deduplicate
        if lhs > rhs:
            lhs, rhs = rhs, lhs
        results.append((lhs, rhs))
    return sorted(set(results))


def format_equation(lhs, rhs):
    lhs_num, lhs_den = lhs
    rhs_num, rhs_den = rhs
    return f"{format_side(lhs_num, lhs_den)} = {format_side(rhs_num, rhs_den)}"


def main():
    if len(sys.argv) != 2 or ':' not in sys.argv[1]:
        print("Usage: python comma_equations.py N:D")
        print("Example: python comma_equations.py 169:168")
        sys.exit(1)

    n, d = map(int, sys.argv[1].split(':'))
    if n < d:
        n, d = d, n

    print(f"Comma: {n}:{d}")
    print(f"  {n} = {factorint(n)}")
    print(f"  {d} = {factorint(d)}")

    prime_info = comma_to_prime_degrees(n, d)
    primes = sorted(prime_info.keys())

    lhs_num = [(p, prime_info[p][0]) for p in primes if prime_info[p][1] == 'L']
    rhs_num = [(p, prime_info[p][0]) for p in primes if prime_info[p][1] == 'R']
    simplest = ((tuple(lhs_num), ()), (tuple(rhs_num), ()))
    print(f"\nSimplest equation (ignoring 2): {format_equation(*simplest)}")
    degrees = {p: prime_info[p][0] for p in primes}
    print(f"Degrees: {', '.join(f'degree({p})={degrees[p]}' for p in primes)}")

    equations = all_least_degree_equations(prime_info)
    print(f"\nAll {len(equations)} least-degree equations:\n")
    for lhs, rhs in equations:
        print(f"  {format_equation(lhs, rhs)}")


if __name__ == "__main__":
    main()
