import { mutationOptions, type QueryClient } from "@tanstack/react-query";

import type {
  CachePruneReport,
  JoinedRoom,
  RoomPreparationSummary,
  RoomProfileSummary,
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
    mutationOptions<JoinedRoom, unknown, { roomId: string; password: string }>({
      mutationFn: mutationFn<JoinedRoom, unknown, { roomId: string; password: string }>(
        ({ roomId, password }) => api.rooms.createRemote(roomId, password),
      ),
      onSuccess: (room) => invalidateRoom(client, room.roomId),
    }),

  joinRemote: (client: QueryClient) =>
    mutationOptions<JoinedRoom, unknown, { roomId: string; password: string }>({
      mutationFn: mutationFn<JoinedRoom, unknown, { roomId: string; password: string }>(
        ({ roomId, password }) => api.rooms.joinRemote(roomId, password),
      ),
      onSuccess: (room) => invalidateRoom(client, room.roomId),
    }),

  syncRemote: (client: QueryClient) =>
    mutationOptions<unknown, unknown, string>({
      mutationFn: mutationFn<unknown, unknown, string>((roomId) =>
        api.rooms.syncRemote(roomId),
      ),
      onSuccess: (_res, roomId) => invalidateRoom(client, roomId),
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
      mutationFn: mutationFn<boolean, unknown, string>(api.rooms.leave),
      onSuccess: (_removed, roomId) => invalidateRoom(client, roomId),
    }),

  prepare: (client: QueryClient) =>
    mutationOptions<RoomPreparationSummary, unknown, string>({
      mutationFn: mutationFn<RoomPreparationSummary, unknown, string>(api.rooms.prepareRevision),
      onSuccess: (prepared) => invalidateRoom(client, prepared.roomId),
    }),

  createProfile: (client: QueryClient) =>
    mutationOptions<RoomProfileSummary, unknown, string>({
      mutationFn: mutationFn<RoomProfileSummary, unknown, string>(api.rooms.createProfile),
      onSuccess: (profile) => invalidateRoom(client, profile.roomId),
    }),

  pruneCache: (client: QueryClient) =>
    mutationOptions<CachePruneReport, unknown, void>({
      mutationFn: mutationFn<CachePruneReport, unknown, void>(api.rooms.pruneCache),
      onSuccess: () => client.invalidateQueries({ queryKey: roomKeys.cache() }),
    }),
} as const;
