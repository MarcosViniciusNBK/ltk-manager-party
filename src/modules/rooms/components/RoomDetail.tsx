import {
  ArrowClockwiseIcon,
  ArrowRightIcon,
  CheckCircleIcon,
  CloudArrowDownIcon,
  PaperPlaneTiltIcon,
  SignOutIcon,
  UsersThreeIcon,
  WarningCircleIcon,
} from "@phosphor-icons/react";
import { useNavigate } from "@tanstack/react-router";
import { useEffect, useState } from "react";

import {
  Button,
  Code,
  EmptyState,
  Progress,
  SectionCard,
  SelectField,
  useToast,
} from "@/components";
import { errorMessage, errorTitle, m } from "@/i18n";
import type { RoomPublishProgress, TransferProgress } from "@/lib/bindings";
import type { RemoteMemberInfo } from "@/lib/bindings.gen";
import { useTauriEvent } from "@/lib/useTauriEvent";
import { useActiveProfile, useProfiles } from "@/modules/library";
import { formatBytes } from "@/utils";

import {
  useLeaveRoom,
  usePublishRoomProfile,
  useRemoteRoomMembers,
  useRoomLocalStatus,
  useRoomManifest,
  useRoomMemberships,
  useRoomSnapshot,
  useSyncRoomProfile,
} from "../api";
import { getRoomWorkflowStatus } from "../status";

export function RoomDetail({ roomId }: { roomId: string }) {
  const navigate = useNavigate();
  const toast = useToast();
  const { data: rooms = [] } = useRoomMemberships();
  const { data: snapshot } = useRoomSnapshot(roomId);
  const { data: manifest } = useRoomManifest(roomId);
  const { data: localStatus } = useRoomLocalStatus(roomId);
  const { data: activeProfile } = useActiveProfile();
  const { data: profiles = [] } = useProfiles();
  const { data: members = [] } = useRemoteRoomMembers(roomId);
  const leaveRoom = useLeaveRoom();
  const publishProfile = usePublishRoomProfile();
  const retrySync = useSyncRoomProfile();
  const [recoveryProfileId, setRecoveryProfileId] = useState("");
  const [downloadProgress, setDownloadProgress] = useState<Record<string, TransferProgress>>({});
  const [uploadProgress, setUploadProgress] = useState<Record<string, TransferProgress>>({});
  const [publishProgress, setPublishProgress] = useState<RoomPublishProgress | null>(null);
  const membership = rooms.find((room) => room.roomId === roomId);
  const workflow = getRoomWorkflowStatus({
    phase: snapshot?.phase,
    activeRevision: snapshot?.activeRevision,
    preparedRevision: localStatus?.preparedRevision,
    profileRevision: localStatus?.profile?.revision,
    roomProfileId: localStatus?.profile?.localProfileId,
    activeProfileId: activeProfile?.id,
  });
  const progress = snapshot?.totalBlobs
    ? Math.round((snapshot.verifiedBlobs / snapshot.totalBlobs) * 100)
    : 0;

  useEffect(() => {
    if (!recoveryProfileId && activeProfile) setRecoveryProfileId(activeProfile.id);
  }, [activeProfile, recoveryProfileId]);

  useEffect(() => {
    setDownloadProgress({});
    setUploadProgress({});
    setPublishProgress(null);
  }, [roomId]);

  useTauriEvent<TransferProgress>("room-transfer-progress", (transfer) => {
    if (transfer.roomId !== roomId) return;
    const update = (current: Record<string, TransferProgress>) => ({
      ...current,
      [transfer.contentHash]: transfer,
    });
    if (transfer.direction === "download") setDownloadProgress(update);
    if (transfer.direction === "upload") setUploadProgress(update);
  });

  useTauriEvent<RoomPublishProgress>("room-publish-progress", (publication) => {
    if (publication.roomId !== roomId) return;
    if (publication.stage === "preparing") setUploadProgress({});
    setPublishProgress(publication);
  });

  const publicationActive =
    publishProfile.isPending ||
    (publishProgress !== null &&
      publishProgress.stage !== "complete" &&
      publishProgress.stage !== "failed");
  const publicationVisible = publicationActive || publishProgress?.stage === "failed";

  function leave() {
    leaveRoom.mutate(roomId, {
      onSuccess: () => toast.success(m.rooms_left_title(), m.rooms_left_description()),
      onError: (error) =>
        toast.error(errorTitle(error, m.rooms_leave_failed_title()), errorMessage(error)),
    });
  }

  function retry() {
    retrySync.mutate(roomId, {
      onSuccess: () => toast.success(m.rooms_sync_done_title(), m.rooms_sync_done_description()),
      onError: (error) =>
        toast.error(errorTitle(error, m.rooms_sync_failed_title()), errorMessage(error)),
    });
  }

  function publishInitialProfile() {
    if (!recoveryProfileId) return;
    publishProfile.mutate(
      { roomId, profileId: recoveryProfileId },
      {
        onSuccess: () =>
          toast.success(m.rooms_publish_done_title(), m.rooms_publish_done_description()),
        onError: (error) =>
          toast.error(errorTitle(error, m.rooms_publish_failed_title()), errorMessage(error)),
      },
    );
  }

  return (
    <div className="space-y-4" data-ui="RoomWorkspace:detail">
      <SectionCard
        title={roomId}
        description={m.rooms_workspace_draft()}
        icon={<UsersThreeIcon className="h-4 w-4" />}
        action={
          <div className="flex items-center gap-2">
            <Button
              variant="outline"
              size="sm"
              left={<ArrowClockwiseIcon weight="bold" />}
              loading={retrySync.isPending}
              onClick={retry}
            >
              {m.rooms_refresh_action()}
            </Button>
            <Button
              variant="ghost"
              size="sm"
              left={<SignOutIcon weight="bold" />}
              loading={leaveRoom.isPending}
              onClick={leave}
            >
              {m.rooms_leave_action()}
            </Button>
          </div>
        }
      >
        <div className="flex flex-wrap items-center justify-between gap-4 rounded-xl border border-surface-700 bg-surface-900/60 p-4">
          <div className="flex items-center gap-3">
            {workflow.profileReady ? (
              <CheckCircleIcon weight="fill" className="h-8 w-8 text-emerald-400" />
            ) : (
              <CloudArrowDownIcon className="text-primary-300 h-8 w-8" />
            )}
            <div>
              <p className="font-semibold text-surface-50">
                {workflow.profileReady
                  ? m.rooms_auto_sync_ready_title()
                  : m.rooms_auto_sync_working_title()}
              </p>
              <p className="text-sm text-surface-400">
                {workflow.profileReady
                  ? m.rooms_auto_sync_ready_description({ revision: snapshot?.activeRevision ?? 0 })
                  : m.rooms_auto_sync_working_description()}
              </p>
            </div>
          </div>
          <div className="flex items-center gap-2">
            <Code>{roomId}</Code>
            <Button
              size="sm"
              variant="outline"
              left={<ArrowRightIcon weight="bold" />}
              disabled={!workflow.profileReady}
              onClick={() => navigate({ to: "/mods" })}
            >
              {m.rooms_open_mods_action()}
            </Button>
          </div>
        </div>
        {!workflow.profileReady && (
          <div className="space-y-2">
            <Progress.Root
              value={progress}
              label={m.rooms_sync_progress_label()}
              valueLabel={m.rooms_sync_progress_value({
                verified: snapshot?.verifiedBlobs ?? 0,
                total: snapshot?.totalBlobs ?? 0,
              })}
            >
              <Progress.Track size="sm">
                <Progress.Indicator />
              </Progress.Track>
            </Progress.Root>
            {(snapshot?.phase === "blocked" || snapshot?.phase === "stale") && (
              <Button
                size="sm"
                variant="outline"
                left={<WarningCircleIcon weight="bold" />}
                loading={retrySync.isPending}
                onClick={retry}
              >
                {m.rooms_retry_sync_action()}
              </Button>
            )}
          </div>
        )}
        {publicationVisible && (
          <div className="border-primary-500/30 bg-primary-500/5 space-y-3 rounded-xl border p-4">
            <p className="text-sm font-medium text-surface-100">
              {publishProgress
                ? publishProgressLabel(publishProgress)
                : m.rooms_publish_progress_preparing()}
            </p>
            {Object.values(uploadProgress).map((transfer) => {
              const uploaded = Number(transfer.transferredBytes);
              const total = Number(transfer.totalBytes);
              const percent = total > 0 ? Math.min(100, Math.round((uploaded / total) * 100)) : 0;
              return (
                <Progress.Root
                  key={transfer.contentHash}
                  value={percent}
                  label={transfer.displayName ?? transfer.contentHash.slice(0, 12)}
                  valueLabel={
                    uploaded >= total
                      ? m.rooms_upload_ready()
                      : m.rooms_upload_progress_value({
                          uploaded: formatBytes(uploaded),
                          total: formatBytes(total),
                        })
                  }
                >
                  <Progress.Track size="sm">
                    <Progress.Indicator />
                  </Progress.Track>
                </Progress.Root>
              );
            })}
          </div>
        )}
        <p className="text-xs text-surface-500">{m.rooms_apply_safety_note()}</p>
      </SectionCard>

      <div className="grid gap-4 xl:grid-cols-[minmax(0,1.35fr)_minmax(16rem,0.65fr)]">
        <SectionCard
          title={m.rooms_manifest_title()}
          description={m.rooms_manifest_description()}
          icon={<CloudArrowDownIcon className="h-4 w-4" />}
        >
          {!manifest ? (
            <div className="space-y-3">
              <EmptyState
                size="sm"
                icon={<CloudArrowDownIcon className="h-12 w-12" />}
                title={m.rooms_manifest_empty_title()}
                description={m.rooms_manifest_empty_description()}
              />
              <div className="flex flex-wrap items-end justify-center gap-3 rounded-lg border border-surface-700/60 bg-surface-800/35 p-3">
                <SelectField
                  className="min-w-56"
                  label={m.rooms_publish_profile_label()}
                  options={profiles.map((profile) => ({
                    value: profile.id,
                    label: profile.name,
                  }))}
                  value={recoveryProfileId}
                  onValueChange={(value) => setRecoveryProfileId(value ?? "")}
                />
                <Button
                  variant="filled"
                  left={<PaperPlaneTiltIcon weight="bold" />}
                  onClick={publishInitialProfile}
                  loading={publishProfile.isPending}
                  disabled={!recoveryProfileId || publicationActive}
                >
                  {m.rooms_publish_action()}
                </Button>
              </div>
            </div>
          ) : (
            <div className="space-y-2">
              {manifest.mods.map((mod) => {
                const transfer = downloadProgress[mod.contentHash];
                const transferred = Number(transfer?.transferredBytes ?? 0);
                const total = Number(transfer?.totalBytes ?? mod.sizeBytes);
                const modProgress =
                  total > 0 ? Math.min(100, Math.round((transferred / total) * 100)) : 0;
                const ready =
                  workflow.profileReady ||
                  localStatus?.cachedContentHashes.includes(mod.contentHash) ||
                  (transfer && transferred >= total);
                return (
                  <div
                    key={mod.contentHash}
                    className="space-y-2 rounded-lg border border-surface-700/60 bg-surface-800/35 p-3"
                  >
                    <div className="flex items-center justify-between gap-3">
                      <div className="min-w-0">
                        <p className="truncate text-sm font-medium text-surface-100">
                          {mod.displayName}
                        </p>
                        <p className="text-xs text-surface-400">{mod.version || mod.format}</p>
                      </div>
                      <span className="shrink-0 text-xs text-surface-400">
                        {formatBytes(Number(mod.sizeBytes))}
                      </span>
                    </div>
                    {!workflow.profileReady && (
                      <Progress.Root
                        value={ready ? 100 : modProgress}
                        label={mod.displayName}
                        valueLabel={
                          ready
                            ? m.rooms_mod_download_ready()
                            : transfer
                              ? m.rooms_mod_download_progress({
                                  downloaded: formatBytes(transferred),
                                  total: formatBytes(total),
                                })
                              : m.rooms_mod_download_waiting()
                        }
                      >
                        <Progress.Track size="sm">
                          <Progress.Indicator />
                        </Progress.Track>
                      </Progress.Root>
                    )}
                  </div>
                );
              })}
              {manifest.mods.length === 0 && (
                <p className="text-sm text-surface-400">{m.rooms_shared_profile_empty()}</p>
              )}
            </div>
          )}
        </SectionCard>

        <SectionCard
          title={m.rooms_members_title()}
          description={m.rooms_members_description()}
          icon={<UsersThreeIcon className="h-4 w-4" />}
        >
          <div className="space-y-2">
            {members.map((member: RemoteMemberInfo) => {
              const isYou = membership?.memberId === member.memberId;
              return (
                <div
                  key={member.memberId}
                  className="flex items-center justify-between gap-3 rounded-lg border border-surface-700/60 bg-surface-800/35 p-3"
                >
                  <div className="flex min-w-0 items-center gap-2">
                    <span
                      className={`h-2.5 w-2.5 shrink-0 rounded-full ${member.isOnline ? "bg-emerald-400" : "bg-surface-500"}`}
                    />
                    <div className="min-w-0">
                      <p className="truncate font-mono text-xs text-surface-100">
                        {isYou ? m.rooms_member_you() : member.memberId.slice(0, 12)}
                      </p>
                      <p className="text-xs text-surface-400">
                        rev {member.lastAcknowledgedRevision}
                      </p>
                    </div>
                  </div>
                  {member.lastAcknowledgedRevision === snapshot?.activeRevision && (
                    <CheckCircleIcon weight="fill" className="h-4 w-4 text-emerald-400" />
                  )}
                </div>
              );
            })}
            {members.length === 0 && (
              <p className="text-sm text-surface-400">{m.rooms_member_remote_pending()}</p>
            )}
          </div>
        </SectionCard>
      </div>
    </div>
  );
}

function publishProgressLabel(progress: RoomPublishProgress): string {
  switch (progress.stage) {
    case "preparing":
      return m.rooms_publish_progress_preparing();
    case "uploading":
      return m.rooms_publish_progress_uploading({
        completed: progress.completedMods,
        total: progress.totalMods,
      });
    case "publishing":
      return m.rooms_publish_progress_publishing();
    case "finalizing":
      return m.rooms_publish_progress_finalizing();
    case "complete":
      return m.rooms_publish_progress_complete();
    case "failed":
      return m.rooms_publish_progress_failed();
  }
}
