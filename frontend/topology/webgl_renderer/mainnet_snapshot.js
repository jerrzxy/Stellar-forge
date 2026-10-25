/**
 * mainnet_snapshot – builds a massive synthetic snapshot of the public
 * mainnet quorum topology for validation and benchmarking.
 *
 * Structure mirrors real mainnet: ~100 organizations, each org owning a
 * validator cluster, orgs grouped into quorum slices with threshold-voting
 * and inner (indirect-trust) sub-slices. A configurable fraction of peers are
 * flagged `missing` (referenced but not present) so the red "missing peer"
 * rendering path is exercised at scale.
 *
 * Deterministic: seeded PRNG → identical snapshot on every run (screenshot
 * diffing + reproducible benches).
 *
 * @module topology/webgl_renderer/mainnet_snapshot
 */

/**
 * mulberry32 – tiny deterministic PRNG.
 * @param {number} seed
 */
function mulberry32(seed) {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

const STELLAR_KEY_ALPHABET = 'ABCDEFGHIJKLMNOPQRSTUVWXYZ234567';

/**
 * @param {() => number} rnd
 * @returns {string} G... public key shaped id
 */
function makeKey(rnd) {
  let s = 'G';
  for (let i = 0; i < 55; i++) {
    s += STELLAR_KEY_ALPHABET[Math.floor(rnd() * STELLAR_KEY_ALPHABET.length)];
  }
  return s;
}

/**
 * Generates a quorum-dump shaped snapshot in the per-validator format
 * understood by `parseQuorumDump` (shape 3).
 *
 * @param {{ orgs?: number, validatorsPerOrg?: number, missingFraction?: number, seed?: number }} [opts]
 * @returns {{ validators: Array<object>, meta: object }}
 */
export function buildMainnetSnapshot(opts = {}) {
  const {
    orgs = 100,
    validatorsPerOrg = 20,
    missingFraction = 0.04,
    seed = 288, // issue number :)
  } = opts;

  const rnd = mulberry32(seed);
  /** @type {Array<{public_key: string, name: string, organization: string, quorum_set: any}>} */
  const validators = [];
  /** @type {Map<string, string>} */
  const orgOf = new Map();
  /** @type {string[][]} – member keys grouped per org, in creation order */
  const membersByOrgOrder = [];

  for (let o = 0; o < orgs; o++) {
    const orgName = `Org-${String(o).padStart(3, '0')}`;
    /** @type {string[]} */
    const members = [];
    for (let v = 0; v < validatorsPerOrg; v++) {
      const key = makeKey(rnd);
      members.push(key);
      orgOf.set(key, orgName);
      validators.push({
        public_key: key,
        name: `${orgName}-V${String(v).padStart(2, '0')}`,
        organization: orgName,
        quorum_set: null, // filled below
      });
    }
    membersByOrgOrder.push(members);
  }

  const byKey = new Map(validators.map((v) => [v.public_key, v]));

  // Build org-local quorum sets: threshold ~2/3 of members, split into an
  // outer slice (direct trust) + two inner slices (indirect trust).
  for (const members of membersByOrgOrder) {
    const shuffled = members.slice().sort(() => rnd() - 0.5);
    const outer = shuffled.slice(0, Math.ceil(shuffled.length / 3));
    const innerA = shuffled.slice(Math.ceil(shuffled.length / 3), Math.ceil((shuffled.length * 2) / 3));
    const innerB = shuffled.slice(Math.ceil((shuffled.length * 2) / 3));
    const ownSet = {
      threshold: Math.max(1, Math.ceil(outer.length * (2 / 3))),
      validators: outer,
      inner_quorum_sets: [
        { threshold: Math.max(1, Math.ceil(innerA.length * (2 / 3))), validators: innerA },
        { threshold: Math.max(1, Math.ceil(innerB.length * (2 / 3))), validators: innerB },
      ],
    };
    for (const m of members) {
      const mv = byKey.get(m);
      if (mv) mv.quorum_set = ownSet;
    }
  }

  // Cross-org trust: each org also trusts ~15 peers in other orgs (the
  // org-level topology) – direct edges between org members.
  const orgKeys = [...new Set(orgOf.values())];
  /** @type {Map<string, string[]>} */
  const membersByOrg = new Map();
  for (const [k, org] of orgOf) {
    if (!membersByOrg.has(org)) membersByOrg.set(org, []);
    membersByOrg.get(org).push(k);
  }
  for (const [org, members] of membersByOrg) {
    const targets = [];
    for (let i = 0; i < 15; i++) {
      const pick = orgKeys[Math.floor(rnd() * orgKeys.length)];
      if (pick === org) continue;
      const pool = membersByOrg.get(pick);
      if (pool) targets.push(pool[Math.floor(rnd() * pool.length)]);
    }
    for (const m of members) {
      const mv = byKey.get(m);
      if (!mv || !mv.quorum_set) continue;
      mv.quorum_set = {
        ...mv.quorum_set,
        validators: [...mv.quorum_set.validators, ...targets],
      };
    }
  }

  // Inject missing peers: referenced keys with no validator record.
  const allMembers = [...membersByOrg.values()].flat();
  const missingCount = Math.floor(allMembers.length * missingFraction);
  for (let i = 0; i < missingCount; i++) {
    const victim = byKey.get(allMembers[Math.floor(rnd() * allMembers.length)]);
    if (!victim || !victim.quorum_set) continue;
    victim.quorum_set = {
      ...victim.quorum_set,
      validators: [...victim.quorum_set.validators, makeKey(rnd)],
    };
  }

  return {
    validators,
    meta: {
      source: 'synthetic-mainnet',
      orgs,
      validators: validators.length,
      missingTarget: missingCount,
      seed,
      generatedAt: new Date(0).toISOString(), // fixed → deterministic fixture
    },
  };
}
