/**
 * Backend entry point used by every view.
 *
 * In development `vite.config.ts` aliases `@/lib/backend` to
 * `@/lib/backend.mock.ts` when `VITE_MOCK=1`, so the in-memory double never
 * reaches a production bundle.
 */
export { api, events } from "@/lib/api";
