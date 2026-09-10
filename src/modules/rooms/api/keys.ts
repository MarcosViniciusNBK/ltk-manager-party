/** Cached data owned by the room workspace. */
export const roomKeys = {
  all: ["rooms"] as const,
  memberships: () => [...roomKeys.all, "memberships"] as const,
  snapshot: (roomId: string) => [...roomKeys.all, "snapshot", roomId] as const,
  manifest: (roomId: string) => [...roomKeys.all, "manifest", roomId] as const,
  localStatus: (roomId: string) => [...roomKeys.all, "local-status", roomId] as const,
  members: (roomId: string) => [...roomKeys.all, "members", roomId] as const,
  cache: () => [...roomKeys.all, "cache"] as const,
};
