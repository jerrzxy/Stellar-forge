/**
 * auth_validator.ts — public entry point for the Smart Contract Auth Hierarchy
 * validation engine (issue #228).
 *
 * Re-exports the canonical implementation living in
 * `frontend/builder/auth_tree/src/lib/auth_validator.ts` so that any
 * consumer (CLI, tests, other packages) can import from a stable,
 * top-level path without depending on the builder package internals.
 */

/**
 * auth_validator.ts — public entry point for the Smart Contract Auth Hierarchy
 * validation engine (issue #228).
 *
 * This is a re-export of the canonical implementation that lives in
 * `frontend/builder/auth_tree/src/lib/auth_validator.ts`. Importing from this
 * path gives consumers a stable, top-level API surface.
 */

export type { AuthTreeNode, AuthPayload, AuthAction, ValidationResult } from '../builder/auth_tree/src/lib/auth_validator';
export { validateAuthTree, generateAuthPayload, createAliceRouterLiquidityPoolTree, createDeepAuthTree } from '../builder/auth_tree/src/lib/auth_validator';
