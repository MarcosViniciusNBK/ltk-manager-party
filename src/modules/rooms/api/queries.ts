import { queryOptions, useQuery } from "@tanstack/react-query";

import type {
  JoinedRoom,
  RemoteMemberInfo,
  RoomCacheStatus,
  RoomLocalStatus,
  RoomManifest,
  RoomSyncSnapshot,
} from "@/lib/bindings.gen";
import { api } from "@/lib/tauri";
import { queryFn, queryFnWithArgs } from "@/utils";

import { roomKeys } from "./keys";

/** Read-only facts that make up a local room workspace. */
export const roomQueries = {
  memberships: () =>
    queryOptions<JoinedRoom[]>({
      queryKey: roomKeys.memberships(),
      queryFn: queryFn(api.rooms.listMemberships),
    }),

  snapshot: (roomId: string) =>
    queryOptions<RoomSyncSnapshot>({
      queryKey: roomKeys.snapshot(roomId),
      queryFn: queryFnWithArgs(api.rooms.snapshot, roomId),
      enabled: roomId.length > 0,
      refetchInterval: 4000,
    }),

  manifest: (roomId: string) =>
    queryOptions<RoomManifest | null>({
      queryKey: roomKeys.manifest(roomId),
      queryFn: queryFnWithArgs(api.rooms.acceptedManifest, roomId),
      enabled: roomId.length > 0,
      refetchInterval: 4000,
    }),

  localStatus: (roomId: string) =>
    queryOptions<RoomLocalStatus>({
      queryKey: roomKeys.localStatus(roomId),
      queryFn: queryFnWithArgs(api.rooms.localStatus, roomId),
      enabled: roomId.length > 0,
      refetchInterval: 4000,
    }),

  remoteMembers: (roomId: string) =>
    queryOptions<RemoteMemberInfo[]>({
      queryKey: roomKeys.members(roomId),
      queryFn: queryFnWithArgs(api.rooms.getRemoteMembers, roomId),
      enabled: roomId.length > 0,
      refetchInterval: 5000,
    }),

  cache: () =>
    queryOptions<RoomCacheStatus>({
      queryKey: roomKeys.cache(),
      queryFn: queryFn(api.rooms.cacheStatus),
    }),
} as const;

export function useRoomMemberships() {
  return useQuery(roomQueries.memberships());
}

export function useRoomSnapshot(roomId: string) {
  return useQuery(roomQueries.snapshot(roomId));
}

export function useRoomManifest(roomId: string) {
  return useQuery(roomQueries.manifest(roomId));
}

export function useRoomLocalStatus(roomId: string) {
  return useQuery(roomQueries.localStatus(roomId));
}

export function useRemoteRoomMembers(roomId: string) {
  return useQuery(roomQueries.remoteMembers(roomId));
}

export function useRoomCacheStatus() {
  return useQuery(roomQueries.cache());
}
