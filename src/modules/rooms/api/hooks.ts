import { useMutation, useQueryClient } from "@tanstack/react-query";

import { roomMutations } from "./mutations";

export function useCreateRemoteRoom() {
  return useMutation(roomMutations.createRemote(useQueryClient()));
}

export function useJoinRemoteRoom() {
  return useMutation(roomMutations.joinRemote(useQueryClient()));
}

export function usePublishRoomProfile() {
  return useMutation(roomMutations.publishProfile(useQueryClient()));
}

export function useSyncRoomProfile() {
  return useMutation(roomMutations.syncProfile(useQueryClient()));
}

export function useCreateRoomDraft() {
  return useMutation(roomMutations.createDraft(useQueryClient()));
}

export function useOpenRoomDraft() {
  return useMutation(roomMutations.openDraft(useQueryClient()));
}

export function useLeaveRoom() {
  return useMutation(roomMutations.leave(useQueryClient()));
}

export function usePruneRoomCache() {
  return useMutation(roomMutations.pruneCache(useQueryClient()));
}
