/** Whether `v` is an IPv4 or IPv6 address, or a CIDR block of one (`10.0.0.0/8`, `2001:db8::/32`). */
export function isCidr(v: string): boolean {
  const [addr, bits, extra] = v.trim().split("/");
  if (!addr || extra !== undefined) return false;
  const v4 = isV4(addr);
  const v6 = !v4 && isV6(addr);
  if (!v4 && !v6) return false;
  if (bits === undefined) return true;
  if (!/^\d{1,3}$/.test(bits)) return false;
  return Number(bits) <= (v4 ? 32 : 128);
}

function isV4(a: string): boolean {
  const parts = a.split(".");
  return parts.length === 4 && parts.every((p) => /^\d{1,3}$/.test(p) && Number(p) <= 255);
}

function isV6(a: string): boolean {
  if (!/^[0-9a-fA-F:.]+$/.test(a) || a.includes(":::")) return false;
  const doubles = a.split("::").length - 1;
  if (doubles > 1) return false;
  let groups = a.split(":");
  // An embedded IPv4 tail counts as two groups.
  const last = groups[groups.length - 1] ?? "";
  let width = 8;
  if (last.includes(".")) {
    if (!isV4(last)) return false;
    groups = groups.slice(0, -1);
    width = 6;
  }
  const real = groups.filter((g) => g !== "");
  if (!real.every((g) => /^[0-9a-fA-F]{1,4}$/.test(g))) return false;
  if (doubles === 1) return real.length < width;
  return real.length === width && !a.startsWith(":") && !a.endsWith(":");
}
