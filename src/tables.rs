//! The constants of the DEFLATE format (RFC 1951) and a table of logarithms.

/// Literal and length symbols: 0..=255 are bytes, 256 ends a block, 257..=285
/// are match lengths.  286 and 287 exist only in the fixed code.
pub(crate) const NUM_LL: usize = 288;
pub(crate) const USED_LL: usize = 286;
/// Offset symbols.  30 and 31 exist only in the fixed code.
pub(crate) const NUM_OFF: usize = 32;
pub(crate) const USED_OFF: usize = 30;
pub(crate) const EOB: usize = 256;
pub(crate) const FIRST_LEN: usize = 257;
pub(crate) const NUM_LEN: usize = 29;
pub(crate) const MIN_MATCH: usize = 3;
pub(crate) const MAX_MATCH: usize = 258;

/// The longest codeword this encoder gives a literal or length symbol.  The
/// format allows 15; with 14, four literals and the up to 7 bits left over
/// from the last flush always fit in the 64-bit buffer (7 + 4 * 14 = 63), so
/// the literal loop flushes once per four bytes without testing for room.
pub(crate) const MAX_LL_BITS: usize = 14;
pub(crate) const MAX_OFF_BITS: usize = 15;
pub(crate) const MAX_PRE_BITS: usize = 7;

pub(crate) const LEN_BASE: [u16; NUM_LEN] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
pub(crate) const LEN_EXTRA: [u8; NUM_LEN] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
pub(crate) const OFF_EXTRA: [u8; USED_OFF] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

/// The order in which a block header lists the lengths of the code that
/// encodes the code lengths.
pub(crate) const PRE_ORDER: [u8; 19] =
    [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];

/// Match length -> index of its length symbol (symbol 257 + index).
pub(crate) const LEN_SLOT: [u8; MAX_MATCH + 1] = {
    let mut t = [0u8; MAX_MATCH + 1];
    let mut len = MIN_MATCH;
    while len <= MAX_MATCH {
        let mut s = NUM_LEN - 1;
        while LEN_BASE[s] as usize > len {
            s -= 1;
        }
        t[len] = s as u8;
        len += 1;
    }
    t
};

/// round(256 * log2(f)) for f in 1..=255; the entry for 0 is 0.
pub(crate) static LOG2_Q8: [u16; 256] = [
    0, 0, 256, 406, 512, 594, 662, 719, 768, 812, 850, 886,
    918, 947, 975, 1000, 1024, 1046, 1068, 1087, 1106, 1124, 1142, 1158,
    1174, 1189, 1203, 1217, 1231, 1244, 1256, 1268, 1280, 1291, 1302, 1313,
    1324, 1334, 1343, 1353, 1362, 1372, 1380, 1389, 1398, 1406, 1414, 1422,
    1430, 1437, 1445, 1452, 1459, 1466, 1473, 1480, 1487, 1493, 1500, 1506,
    1512, 1518, 1524, 1530, 1536, 1542, 1547, 1553, 1558, 1564, 1569, 1574,
    1580, 1585, 1590, 1595, 1599, 1604, 1609, 1614, 1618, 1623, 1628, 1632,
    1636, 1641, 1645, 1649, 1654, 1658, 1662, 1666, 1670, 1674, 1678, 1682,
    1686, 1690, 1693, 1697, 1701, 1705, 1708, 1712, 1715, 1719, 1722, 1726,
    1729, 1733, 1736, 1739, 1743, 1746, 1749, 1752, 1756, 1759, 1762, 1765,
    1768, 1771, 1774, 1777, 1780, 1783, 1786, 1789, 1792, 1795, 1798, 1801,
    1803, 1806, 1809, 1812, 1814, 1817, 1820, 1822, 1825, 1828, 1830, 1833,
    1836, 1838, 1841, 1843, 1846, 1848, 1851, 1853, 1855, 1858, 1860, 1863,
    1865, 1867, 1870, 1872, 1874, 1877, 1879, 1881, 1884, 1886, 1888, 1890,
    1892, 1895, 1897, 1899, 1901, 1903, 1905, 1908, 1910, 1912, 1914, 1916,
    1918, 1920, 1922, 1924, 1926, 1928, 1930, 1932, 1934, 1936, 1938, 1940,
    1942, 1944, 1946, 1947, 1949, 1951, 1953, 1955, 1957, 1959, 1961, 1962,
    1964, 1966, 1968, 1970, 1971, 1973, 1975, 1977, 1978, 1980, 1982, 1984,
    1985, 1987, 1989, 1990, 1992, 1994, 1995, 1997, 1999, 2000, 2002, 2004,
    2005, 2007, 2008, 2010, 2012, 2013, 2015, 2016, 2018, 2020, 2021, 2023,
    2024, 2026, 2027, 2029, 2030, 2032, 2033, 2035, 2036, 2038, 2039, 2041,
    2042, 2044, 2045, 2047,
];

/// f * log2(f) in 1/256 bit.  Slightly low for f >= 256, which is fine for
/// what it is used for: an estimate of how well codes can fit.
#[inline]
pub(crate) fn flog2(f: u32) -> u32 {
    if f < 256 {
        return f * LOG2_Q8[f as usize] as u32;
    }
    let k = 24 - f.leading_zeros(); // f >> k is in 128..=255
    f * ((k << 8) + LOG2_Q8[(f >> k) as usize] as u32)
}
