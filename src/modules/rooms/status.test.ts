import { describe, expect, it } from "vitest";

import { getRoomWorkflowStatus } from "./status";

describe("getRoomWorkflowStatus", () => {
  it("does not mistake an empty local draft for a synchronized revision", () => {
    expect(
      getRoomWorkflowStatus({
        phase: "idle",
        activeRevision: 0,
        preparedRevision: null,
        profileRevision: null,
        roomProfileId: undefined,
        activeProfileId: undefined,
      }),
    ).toEqual({
      synchronized: false,
      prepared: false,
      profileReady: false,
      profileSelected: false,
    });
  });

  it("requires every local result to match the accepted revision", () => {
    expect(
      getRoomWorkflowStatus({
        phase: "synchronized",
        activeRevision: 4,
        preparedRevision: 3,
        profileRevision: 3,
        roomProfileId: "profile-a",
        activeProfileId: "profile-a",
      }),
    ).toEqual({
      synchronized: true,
      prepared: false,
      profileReady: false,
      profileSelected: false,
    });
  });

  it("keeps profile selection separate from applying through Start or Play", () => {
    expect(
      getRoomWorkflowStatus({
        phase: "synchronized",
        activeRevision: 4,
        preparedRevision: 4,
        profileRevision: 4,
        roomProfileId: "profile-a",
        activeProfileId: "profile-a",
      }),
    ).toEqual({
      synchronized: true,
      prepared: true,
      profileReady: true,
      profileSelected: true,
    });
  });
});
