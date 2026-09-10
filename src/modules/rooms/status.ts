export interface RoomWorkflowInput {
  phase: string | undefined;
  activeRevision: number | undefined;
  preparedRevision: number | null | undefined;
  profileRevision: number | null | undefined;
  roomProfileId: string | undefined;
  activeProfileId: string | undefined;
}

export interface RoomWorkflowStatus {
  synchronized: boolean;
  prepared: boolean;
  profileReady: boolean;
  profileSelected: boolean;
}

/**
 * Preparation only counts when it is for the currently accepted revision. A selected profile is
 * intentionally not called applied: application remains the existing, separate Start/Play action.
 */
export function getRoomWorkflowStatus(input: RoomWorkflowInput): RoomWorkflowStatus {
  const synchronized = input.phase === "synchronized" && (input.activeRevision ?? 0) > 0;
  const prepared =
    synchronized &&
    input.preparedRevision !== null &&
    input.preparedRevision === input.activeRevision;
  const profileReady =
    prepared && input.profileRevision !== null && input.profileRevision === input.activeRevision;
  const profileSelected =
    profileReady &&
    input.roomProfileId !== undefined &&
    input.activeProfileId !== undefined &&
    input.roomProfileId === input.activeProfileId;

  return { synchronized, prepared, profileReady, profileSelected };
}
