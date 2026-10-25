/**
 * Auth Validator for Soroban Authorization Hierarchies
 *
 * Validates that constructed auth trees satisfy Soroban's require_auth framework
 * requirements for nested authorization trees.
 */

/**
 * A node in the authorization tree.
 *
 * A node can be an on-chain address (user or contract) or a contract that
 * further delegates to sub-contracts. The tree naturally supports arbitrary
 * depth because each `contract` node may itself contain `subContracts`.
 */
export interface AuthTreeNode {
  /** Stable id used by the builder to track node identity across edits. */
  id: string;
  /** Human-readable label, surfaced in the tree view and validation messages. */
  label: string;
  /** Whether this node is an on-chain address or a contract. */
  type: 'address' | 'contract';
  /** Soroban public key hash (G...) when `type` is `address`. */
  address?: string;
  /** On-chain contract id when `type` is `contract`. */
  contractId?: string;
  /** Nested contracts that this node authorizes (only for `contract` nodes). */
  subContracts?: AuthTreeNode[];
  /** Depth in the tree (root = 0). Maintained by the builder for layout. */
  depth: number;
}

/**
 * The JSON payload that soroban-cli consumes to sign and execute a multi-step
 * transaction whose authorization structure matches `AuthTreeNode`.
 *
 * This shape mirrors the require_auth authorization envelope used by the
 * Soroban SDK: the root is the immediate caller, `actions` enumerates every
 * principal that must authorize, and `resource` identifies the protected
 * resource each authority gates.
 */
export interface AuthPayload {
  /** The principal that is the immediate caller / source of the invocation. */
  source: string;
  /** Authorization entries, one per address that must authorize. */
  actions: AuthAction[];
  /** Protected resource being accessed (contract id or ledger entry). */
  resource: string;
}

/**
 * A single authorization action inside `AuthPayload`.
 */
export interface AuthAction {
  /** Address that must authorize. */
  address: string;
  /** Permission scope for this authorization. */
  permission: 'invoke' | 'edit' | 'admin';
}

/**
 * Outcome of validating an `AuthTreeNode`.
 */
export interface ValidationResult {
  /** Whether the tree meets Soroban's minimum authorization requirements. */
  valid: boolean;
  /** Human-readable reasons the tree is invalid (empty when `valid`). */
  errors: string[];
  /** Populated auth payload when the tree is valid. */
  payload: AuthPayload | null;
}

/**
 * Validate a constructed auth tree against Soroban's authorization requirements.
 *
 * Returns `valid: true` when the tree:
 *   - Has at least one address leaf that will authorize the invocation,
 *   - Has a labeled root, and
 *   - Contains no structural issues (missing labels, cycles, etc.).
 *
 * Cycles are detected via a visited-set so malformed graphs are rejected.
 */
export function validateAuthTree(root: AuthTreeNode): ValidationResult {
  const errors: string[] = [];
  const actions: AuthAction[] = [];
  let deepestContractId: string | undefined;
  let deepestContractDepth = -1;

  function walk(
    node: AuthTreeNode,
    depth: number,
    visited: ReadonlySet<string>,
  ): boolean {
    if (!node) {
      errors.push('Encountered a null node in the tree');
      return false;
    }

    if (visited.has(node.id)) {
      errors.push(`Cycle detected at node "${node.label}" (id=${node.id})`);
      return false;
    }

    if (!node.label) {
      errors.push(`Node ${node.id} is missing a label`);
      return false;
    }

    if (node.type === 'contract') {
      // Track the deepest (most-specific) contract — this is the protected
      // resource being accessed.
      if (depth > deepestContractDepth) {
        deepestContractDepth = depth;
        deepestContractId = node.contractId ?? node.id;
      }
      node.subContracts?.forEach((child) => {
        if (!walk(child, depth + 1, new Set([...visited, node.id]))) {
          errors.push(
            `Invalid sub-contract under node "${node.label}" (id=${node.id})`,
          );
        }
      });
    } else if (node.type === 'address') {
      if (!node.address) {
        errors.push(
          `Address node "${node.label}" (id=${node.id}) is missing its address`,
        );
      } else if (!SorobanAddressPattern.test(node.address)) {
        errors.push(
          `Address node "${node.label}" has a malformed address: ${node.address}`,
        );
      } else {
        actions.push({ address: node.address, permission: inferPermission(node) });
      }
    } else {
      errors.push(`Unknown node type "${node.type}" on node "${node.label}"`);
    }

    return true;
  }

  walk(root, 0, new Set());

  let payload: AuthPayload | null = null;

  const contractRoot = firstContractAncestor(root);
  if (!contractRoot && !root.type) {
    errors.push('Auth tree must have at least one root node');
  } else if (!contractRoot && root.type !== 'contract' && root.type !== 'address') {
    errors.push('Auth tree must have at least one root node');
  } else if (!root.label) {
    errors.push('Auth tree must have at least one root node with a label');
  }

  if (errors.length === 0) {
    const source = contractRoot?.address ?? contractRoot?.contractId ?? root.address ?? root.contractId ?? root.label;
    payload = {
      source,
      actions: sortActions(actions),
      resource: deepestContractId ?? contractRoot?.contractId ?? root.contractId ?? 'default-resource',
    };
  }

  if (actions.length === 0 && !errors.some((e) => e.includes('No authorization actions'))) {
    errors.push('No authorization actions defined in the tree');
    payload = null;
  }

  return { valid: errors.length === 0, errors, payload };
}

/**
 * Generate the JSON payload that soroban-cli will sign and submit.
 *
 * Returns `null` when the tree is invalid; callers should surface the
 * validation errors (available via `validateAuthTree`) to the user.
 *
 * The returned shape matches the `AuthPayload` interface. To compare against
 * SDK output, run the equivalent SDK authorization builder for the same tree
 * and diff the serialized JSON field-by-field (source, actions, resource).
 *
 * Known-valid expected payload for the Alice->Router->Liquidity Pool scenario:
 *   {
 *     "source": "router-id",
 *     "actions": [{"address": "GABCDEFGHIJKLMNOPQRSTUVWXYZ1234567", "permission": "invoke"}],
 *     "resource": "pool-id"
 *   }
 *
 * A full SDK-backed round-trip against soroban-cli requires @stellar/stellar-sdk
 * and a live network stub; that step is not available in this repo without the
 * SDK dependency.
 */
export function generateAuthPayload(root: AuthTreeNode): AuthPayload | null {
  const result = validateAuthTree(root);
  if (!result.valid) {
    console.warn('Auth tree validation failed:', result.errors);
    return null;
  }
  if (!result.payload) {
    console.warn('Failed to generate auth payload');
    return null;
  }
  return result.payload;
}

/**
 * Alice -> Router -> Liquidity Pool authorization scenario.
 *
 * This is the reference scenario called out in the issue spec: Alice calls
 * a Router contract that, in turn, calls a Liquidity Pool contract. The
 * generated payload is verified by the unit tests against a known-valid shape.
 */
export function createAliceRouterLiquidityPoolTree(): AuthTreeNode {
  const aliceAddress = 'GABCDEFGHIJKLMNOPQRSTUVWXYZ1234567';

  return {
    id: 'router-1',
    label: 'Router Contract',
    type: 'contract',
    contractId: 'router-id',
    depth: 0,
    subContracts: [
      {
        id: 'pool-1',
        label: 'Liquidity Pool',
        type: 'contract',
        contractId: 'pool-id',
        depth: 1,
        subContracts: [
          {
            id: 'alice-1',
            label: 'Alice',
            type: 'address',
            address: aliceAddress,
            depth: 2,
          },
        ],
      },
    ],
  };
}

/**
 * Build an auth tree of the requested depth for stress-testing the validator
 * and the tree renderer's zoom/pan behavior.
 */
export function createDeepAuthTree(depth: number): AuthTreeNode {
  function build(remaining: number, label: string): AuthTreeNode {
    if (remaining <= 0) {
      return {
        id: `leaf-${Math.random().toString(36).slice(2, 11)}`,
        label,
        type: 'address' as const,
        address: `G${Math.random().toString(36).slice(2).padStart(55, 'A').slice(0, 55)}`,
        depth: 0,
      };
    }
    return {
      id: `node-${Math.random().toString(36).slice(2, 11)}`,
      label,
      type: 'contract' as const,
      contractId: `CCNTH${Math.random().toString(36).slice(2, 52).padEnd(55, 'X')}`,
      depth: 0,
      subContracts: [build(remaining - 1, `Lvl${remaining - 1}`)],
    };
  }

  return build(depth, `Root depth ${depth}`);
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

const SorobanAddressPattern = /^[GOP][A-Za-z0-9]{1,58}$/;

function inferPermission(node: AuthTreeNode): AuthAction['permission'] {
  // Contracts generally need 'invoke'; admin-level addresses get 'admin'.
  // For the builder's default model, all address leaves default to 'invoke'.
  return 'invoke';
}

function firstContractAncestor(node: AuthTreeNode): AuthTreeNode | undefined {
  if (node.type === 'contract') return node;
  for (const child of node.subContracts ?? []) {
    const found = firstContractAncestor(child);
    if (found) return found;
  }
  return undefined;
}

function sortActions(actions: AuthAction[]): AuthAction[] {
  return [...actions].sort((a, b) => a.address.localeCompare(b.address));
}
