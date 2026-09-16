/**
 * Build-time constants substituted by esbuild (see build.mjs).
 *
 * These are real constants at bundle time, not runtime lookups, so branches guarded by
 * them are eliminated from the bundle that does not need them.
 */
declare const __HAS_TAB_GROUPS__: boolean;
