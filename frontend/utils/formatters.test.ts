import test from "node:test";
import assert from "node:assert/strict";
import {
    formatAddress,
    formatBytes,
    formatCompactNumber,
    formatCurrency,
    formatDate,
    formatDuration,
    formatNumber,
    formatPercent,
    formatRelativeTime,
    formatStroops,
    formatXLM,
    truncateMiddle,
} from "./formatters.ts";

test("formatNumber formats finite values and rejects invalid numbers", () => {
    assert.equal(formatNumber(1234567.89), "1,234,567.89");
    assert.equal(formatNumber(Number.NaN), "—");
    assert.equal(formatNumber(Number.POSITIVE_INFINITY), "—");
});

test("formatCurrency formats values using the requested currency", () => {
    assert.equal(formatCurrency(1234.5), "$1,234.50");
    assert.equal(formatCurrency(1234.5, "EUR", "en-US"), "€1,234.50");
    assert.equal(formatCurrency(Number.NaN), "—");
});

test("formatCompactNumber abbreviates large values", () => {
    assert.equal(formatCompactNumber(1234567), "1.2M");
    assert.equal(formatCompactNumber(-1500), "-1.5K");
    assert.equal(formatCompactNumber(Number.NaN), "—");
});

test("formatPercent formats percent values with fixed precision", () => {
    assert.equal(formatPercent(25.5), "25.5%");
    assert.equal(formatPercent(25.55, 0), "26%");
    assert.equal(formatPercent(Number.NaN), "—");
});

test("formatBytes converts byte counts to human readable units", () => {
    assert.equal(formatBytes(0), "0 B");
    assert.equal(formatBytes(512), "512 B");
    assert.equal(formatBytes(1024), "1 KB");
    assert.equal(formatBytes(1536), "1.5 KB");
    assert.equal(formatBytes(1048576), "1 MB");
    assert.equal(formatBytes(-1024), "-1 KB");
    assert.equal(formatBytes(Number.NaN), "—");
});

test("formatDuration formats seconds into day, hour, minute and second parts", () => {
    assert.equal(formatDuration(0), "0s");
    assert.equal(formatDuration(59), "59s");
    assert.equal(formatDuration(60), "1m");
    assert.equal(formatDuration(3661), "1h 1m 1s");
    assert.equal(formatDuration(90061), "1d 1h 1m 1s");
    assert.equal(formatDuration(-1), "—");
    assert.equal(formatDuration(Number.NaN), "—");
});

test("formatDate formats valid dates and rejects invalid dates", () => {
    assert.equal(
        formatDate("2026-09-15T14:30:00.000Z", {
            year: "numeric",
            month: "short",
            day: "2-digit",
            timeZone: "UTC",
        }),
        "Sep 15, 2026",
    );
    assert.equal(formatDate("not-a-date"), "—");
});

test("formatRelativeTime reports past and future offsets", () => {
    const now = new Date("2026-09-04T00:00:00.000Z");

    assert.equal(
        formatRelativeTime("2026-09-01T00:00:00.000Z", now),
        "3 days ago",
    );
    assert.equal(
        formatRelativeTime("2026-09-06T00:00:00.000Z", now),
        "in 2 days",
    );
    assert.equal(formatRelativeTime("not-a-date", now), "—");
});

test("truncateMiddle shortens long values while preserving both ends", () => {
    const value = "GABCDEFGHIJKLMNOPQRSTUVWXYZ";

    assert.equal(truncateMiddle(value, 4, 4), "GABC...WXYZ");
    assert.equal(truncateMiddle("short"), "short");
    assert.equal(truncateMiddle(value, 1, 1), "G...Z");
    assert.equal(truncateMiddle(value, 0, 0), "...");
});

test("formatAddress truncates Stellar account addresses", () => {
    const address = "GABCDEFGHIJKLMNOPQRSTUVWXYZ234567890ABCDEFGHIJKLMNOPQRSTUV";

    assert.equal(formatAddress(address), "GABCDE...STUV");
    assert.equal(formatAddress("GABC"), "GABC");
});

test("formatXLM converts stroops to XLM without floating point loss", () => {
    assert.equal(formatXLM(0), "0");
    assert.equal(formatXLM(10_000_000), "1");
    assert.equal(formatXLM(12_345_678n), "1.2345678");
    assert.equal(formatXLM("-1"), "-0.0000001");
    assert.equal(formatXLM(10_000_000, 2), "1");
    assert.equal(formatXLM("not-a-number"), "—");
});

test("formatStroops is an alias for formatXLM", () => {
    assert.equal(formatStroops(25_000_000n), "2.5");
});
