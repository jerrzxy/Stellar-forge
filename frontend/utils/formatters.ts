const BYTE_UNITS: readonly string[] = [
    "B",
    "KB",
    "MB",
    "GB",
    "TB",
    "PB",
];

const STROOPS_PER_XLM = 10_000_000n;

const RELATIVE_TIME_UNITS: ReadonlyArray<{
    unit: Intl.RelativeTimeFormatUnit;
    seconds: number;
}> = [
    { unit: "year", seconds: 31_557_600 },
    { unit: "month", seconds: 2_629_800 },
    { unit: "week", seconds: 604_800 },
    { unit: "day", seconds: 86_400 },
    { unit: "hour", seconds: 3_600 },
    { unit: "minute", seconds: 60 },
    { unit: "second", seconds: 1 },
];

export function formatNumber(
    value: number,
    options: Intl.NumberFormatOptions = {},
    locale: string = "en-US",
): string {
    if (!Number.isFinite(value)) {
        return "—";
    }

    return new Intl.NumberFormat(locale, options).format(value);
}

export function formatCurrency(
    value: number,
    currency: string = "USD",
    locale: string = "en-US",
    options: Intl.NumberFormatOptions = {},
): string {
    if (!Number.isFinite(value)) {
        return "—";
    }

    return new Intl.NumberFormat(locale, {
        style: "currency",
        currency,
        ...options,
    }).format(value);
}

export function formatCompactNumber(
    value: number,
    locale: string = "en-US",
): string {
    if (!Number.isFinite(value)) {
        return "—";
    }

    return new Intl.NumberFormat(locale, {
        notation: "compact",
        maximumFractionDigits: 1,
    }).format(value);
}

export function formatPercent(
    value: number,
    fractionDigits: number = 1,
    locale: string = "en-US",
): string {
    if (!Number.isFinite(value)) {
        return "—";
    }

    const digits = Math.max(0, Math.trunc(fractionDigits));

    return new Intl.NumberFormat(locale, {
        style: "percent",
        minimumFractionDigits: digits,
        maximumFractionDigits: digits,
    }).format(value / 100);
}

export function formatBytes(bytes: number, decimals: number = 2): string {
    if (!Number.isFinite(bytes)) {
        return "—";
    }

    if (bytes === 0) {
        return "0 B";
    }

    const digits = Math.max(0, Math.trunc(decimals));
    const exponent = Math.min(
        Math.floor(Math.log(Math.abs(bytes)) / Math.log(1024)),
        BYTE_UNITS.length - 1,
    );
    const value = bytes / 1024 ** exponent;
    const formatted = value
        .toFixed(digits)
        .replace(/\.0+$/, "")
        .replace(/(\.\d*[1-9])0+$/, "$1");

    return `${formatted} ${BYTE_UNITS[exponent]}`;
}

export function formatDuration(seconds: number): string {
    if (!Number.isFinite(seconds) || seconds < 0) {
        return "—";
    }

    const totalSeconds = Math.floor(seconds);
    const parts: string[] = [];
    const units: ReadonlyArray<{ label: string; seconds: number }> = [
        { label: "d", seconds: 86_400 },
        { label: "h", seconds: 3_600 },
        { label: "m", seconds: 60 },
        { label: "s", seconds: 1 },
    ];

    let remaining = totalSeconds;

    for (const { label, seconds: unitSeconds } of units) {
        const amount = Math.floor(remaining / unitSeconds);
        if (amount > 0) {
            parts.push(`${amount}${label}`);
            remaining -= amount * unitSeconds;
        }
    }

    return parts.length > 0 ? parts.join(" ") : "0s";
}

export function formatDate(
    value: Date | string | number,
    options: Intl.DateTimeFormatOptions = {},
    locale: string = "en-US",
): string {
    const date = value instanceof Date ? value : new Date(value);

    if (Number.isNaN(date.getTime())) {
        return "—";
    }

    return new Intl.DateTimeFormat(locale, options).format(date);
}

export function formatRelativeTime(
    value: Date | string | number,
    now: Date = new Date(),
    locale: string = "en-US",
): string {
    const date = value instanceof Date ? value : new Date(value);

    if (Number.isNaN(date.getTime()) || Number.isNaN(now.getTime())) {
        return "—";
    }

    const deltaSeconds = (date.getTime() - now.getTime()) / 1000;
    const absoluteSeconds = Math.abs(deltaSeconds);
    const formatter = new Intl.RelativeTimeFormat(locale, {
        numeric: "auto",
    });

    for (const { unit, seconds } of RELATIVE_TIME_UNITS) {
        if (absoluteSeconds >= seconds || unit === "second") {
            const amount = Math.round(deltaSeconds / seconds);
            return formatter.format(amount, unit);
        }
    }

    return formatter.format(0, "second");
}

export function truncateMiddle(
    value: string,
    start: number = 6,
    end: number = 4,
): string {
    const prefixLength = Math.max(0, Math.trunc(start));
    const suffixLength = Math.max(0, Math.trunc(end));

    if (value.length <= prefixLength + suffixLength + 3) {
        return value;
    }

    const prefix = prefixLength > 0 ? value.slice(0, prefixLength) : "";
    const suffix = suffixLength > 0 ? value.slice(-suffixLength) : "";

    return `${prefix}...${suffix}`;
}

export function formatAddress(
    address: string,
    start: number = 6,
    end: number = 4,
): string {
    return truncateMiddle(address, start, end);
}

export function formatXLM(
    stroops: bigint | number | string,
    maximumFractionDigits: number = 7,
): string {
    const parsed = parseStroops(stroops);

    if (parsed === null) {
        return "—";
    }

    const negative = parsed < 0n;
    const absolute = negative ? -parsed : parsed;
    const whole = absolute / STROOPS_PER_XLM;
    const fraction = absolute % STROOPS_PER_XLM;
    const fractionDigits = Math.min(
        7,
        Math.max(0, Math.trunc(maximumFractionDigits)),
    );
    const fractionText = fraction
        .toString()
        .padStart(7, "0")
        .slice(0, fractionDigits)
        .replace(/0+$/, "");

    const wholeText = new Intl.NumberFormat("en-US", {
        maximumFractionDigits: 0,
    }).format(whole);
    const sign = negative ? "-" : "";

    return fractionText.length > 0
        ? `${sign}${wholeText}.${fractionText}`
        : `${sign}${wholeText}`;
}

export function formatStroops(
    stroops: bigint | number | string,
    maximumFractionDigits: number = 7,
): string {
    return formatXLM(stroops, maximumFractionDigits);
}

function parseStroops(value: bigint | number | string): bigint | null {
    if (typeof value === "bigint") {
        return value;
    }

    if (typeof value === "number") {
        return Number.isFinite(value) ? BigInt(Math.trunc(value)) : null;
    }

    const trimmed = value.trim();

    return /^-?\d+$/.test(trimmed) ? BigInt(trimmed) : null;
}
