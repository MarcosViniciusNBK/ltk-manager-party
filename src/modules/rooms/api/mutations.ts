import { mutationOptions, type QueryClient } from "@tanstack/react-query";

import type {
  CachePruneReport,
  JoinedRoom,
  RoomProfileSummary,
  RoomSyncSnapshot,
} from "@/lib/bindings.gen";
import { api } from "@/lib/tauri";
import { mutationFn } from "@/utils";

import { roomKeys } from "./keys";

function invalidateRoom(client: QueryClient, roomId: string) {
  return Promise.all([
    client.invalidateQueries({ queryKey: roomKeys.memberships() }),
    client.invalidateQueries({ queryKey: roomKeys.snapshot(roomId) }),
    client.invalidateQueries({ queryKey: roomKeys.manifest(roomId) }),
    client.invalidateQueries({ queryKey: roomKeys.localStatus(roomId) }),
    client.invalidateQueries({ queryKey: roomKeys.members(roomId) }),
    client.invalidateQueries({ queryKey: roomKeys.cache() }),
  ]);
}

export const roomMutations = {
  createRemote: (client: QueryClient) =>
    mutationOptions<JoinedRoom, unknown, { roomId: string; password: string; profileId: string }>({
      meta: { silentError: true },
      mutationFn: mutationFn<
        JoinedRoom,
        unknown,
        { roomId: string; password: string; profileId: string }
      >(({ roomId, password, profileId }) => api.rooms.createRemote(roomId, password, profileId)),
      onSettled: (_room, _error, variables) => invalidateRoom(client, variables.roomId),
    }),

  joinRemote: (client: QueryClient) =>
    mutationOptions<JoinedRoom, unknown, { roomId: string; password: string }>({
      meta: { silentError: true },
      mutationFn: mutationFn<JoinedRoom, unknown, { roomId: string; password: string }>(
        ({ roomId, password }) => api.rooms.joinRemote(roomId, password),
      ),
      onSettled: (_room, _error, variables) => invalidateRoom(client, variables.roomId),
    }),

  publishProfile: (client: QueryClient) =>
    mutationOptions<RoomSyncSnapshot, unknown, { roomId: string; profileId: string | null }>({
      meta: { silentError: true },
      mutationFn: mutationFn<
        RoomSyncSnapshot,
        unknown,
        { roomId: string; profileId: string | null }
      >(({ roomId, profileId }) => api.rooms.publishProfile(roomId, profileId)),
      onSuccess: (_snapshot, { roomId }) => invalidateRoom(client, roomId),
    }),

  syncProfile: (client: QueryClient) =>
    mutationOptions<RoomProfileSummary, unknown, string>({
      meta: { silentError: true },
      mutationFn: mutationFn<RoomProfileSummary, unknown, string>((roomId) =>
        api.rooms.syncProfile(roomId),
      ),
      onSuccess: (_profile, roomId) => invalidateRoom(client, roomId),
    }),

  createDraft: (client: QueryClient) =>
    mutationOptions<JoinedRoom, unknown, string>({
      mutationFn: mutationFn<JoinedRoom, unknown, string>(api.rooms.createDraft),
      onSuccess: (room) => invalidateRoom(client, room.roomId),
    }),

  openDraft: (client: QueryClient) =>
    mutationOptions<JoinedRoom, unknown, string>({
      mutationFn: mutationFn<JoinedRoom, unknown, string>(api.rooms.joinDraft),
      onSuccess: (room) => invalidateRoom(client, room.roomId),
    }),

  leave: (client: QueryClient) =>
    mutationOptions<boolean, unknown, string>({
      meta: { silentError: true },
      mutationFn: mutationFn<boolean, unknown, string>(api.rooms.leave),
      onSettled: (_removed, _error, roomId) => invalidateRoom(client, roomId),
    }),

  pruneCache: (client: QueryClient) =>
    mutationOptions<CachePruneReport, unknown, void>({
      meta: { silentError: true },
      mutationFn: mutationFn<CachePruneReport, unknown, void>(api.rooms.pruneCache),
      onSuccess: () => client.invalidateQueries({ queryKey: roomKeys.cache() }),
    }),
} as const;
