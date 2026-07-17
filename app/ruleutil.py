"""Shared rule normalization and stored-rule validation."""
import ipaddress


def _danger_char(ch):
    return (ch in ",\"'\\" or ch.isspace() or ord(ch) <= 0x1f)


def rule_pattern_safe(pattern):
    """Reject classical-rule delimiters, Unicode whitespace, quotes, slash escapes and C0."""
    return isinstance(pattern, str) and not any(_danger_char(ch) for ch in pattern)


def bare(pattern):
    for prefix in ("+.", "*."):
        if pattern.startswith(prefix):
            return pattern[len(prefix):]
    return pattern


def valid_domain(host):
    return (isinstance(host, str) and 0 < len(host) <= 253 and ".." not in host
            and rule_pattern_safe(host))


def norm_domain(token):
    """Normalize a domain token while preserving non-dangerous Unicode."""
    if not rule_pattern_safe(token) or not token:
        return None
    host = token.split("://", 1)[-1].split("/", 1)[0].rsplit("@", 1)[-1]
    host = bare(host.split(":", 1)[0]).strip(".").lower()
    return host if valid_domain(host) else None


def norm_ip(token):
    """Validate IP/CIDR; add /32 or /128 to a bare address."""
    if not rule_pattern_safe(token) or not token:
        return None
    addr = token.split("/", 1)[0]
    if "%" in addr:
        return None
    try:
        ip = ipaddress.ip_address(addr)
    except ValueError:
        return None
    if "/" in token:
        mask = token.split("/", 1)[1]
        if not mask.isdigit():
            return None
        try:
            ipaddress.ip_network(token, strict=False)
        except ValueError:
            return None
        return token
    return token + ("/128" if ip.version == 6 else "/32")


def classify(token):
    if not isinstance(token, str) or not token:
        return None
    addr = token.split("/", 1)[0]
    try:
        ipaddress.ip_address(addr)
    except ValueError:
        host = norm_domain(token)
        return ("domain", host) if host else None
    cidr = norm_ip(token)
    return ("ip", cidr) if cidr else None


def normalize_stored_rule(kind, pattern):
    """Return the sole accepted stored-rule representation, or None."""
    if kind == "domain":
        normalized = norm_domain(pattern)
    elif kind == "ip":
        normalized = norm_ip(pattern)
    else:
        return None
    return (kind, normalized) if normalized else None
