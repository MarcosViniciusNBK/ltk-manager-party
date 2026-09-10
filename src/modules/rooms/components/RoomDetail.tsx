import {
  ArrowRightIcon,
  CheckCircleIcon,
  CloudArrowDownIcon,
  FolderSimpleIcon,
  GameControllerIcon,
  LockKeyIcon,
  SignOutIcon,
  UsersThreeIcon,
} from "@phosphor-icons/react";
import { useNavigate } from "@tanstack/react-router";

import { Button, Code, EmptyState, Progress, SectionCard, useToast } from "@/components";
import { errorMessage, m } from "@/i18n";
import { useActiveProfile } from "@/modules/library";
import { formatBytes } from "@/utils";

import {
  useCreateRoomProfile,
  useLeaveRoom,
  usePrepareRoomRevision,
  useRoomLocalStatus,
  useRoomManifest,
  useRoomMemberships,
  useRoomSnapshot,
} from "../api";
import { getRoomWorkflowStatus } from "../status";
import { WorkflowStep } from "./WorkflowStep";

function phaseLabel(phase: string | undefined): string {
  switch (phase) {
    case "disconnected":
      return m.rooms_sync_phase_disconnected();
    case "connecting":
      return m.rooms_sync_phase_connecting();
    case "comparing":
      return m.rooms_sync_phase_comparing();
    case "transferring":
      return m.rooms_sync_phase_transferring();
    case "verifying":
      return m.rooms_sync_phase_verifying();
    case "synchronized":
      return m.rooms_sync_phase_synchronized();
    case "stale":
      return m.rooms_sync_phase_stale();
    case "blocked":
      return m.rooms_sync_phase_blocked();
    default:
      return m.rooms_sync_phase_unknown();
  }
}

function revisionLabel(revision: number | undefined): string {
  if (!revision) return m.rooms_revision_none();
  return String(revision);
}

export function RoomDetail({ roomId }: { roomId: string }) {
  const navigate = useNavigate();
  const toast = useToast();
  const { data: rooms = [] } = useRoomMemberships();
  const { data: snapshot } = useRoomSnapshot(roomId);
  const { data: manifest } = useRoomManifest(roomId);
  const { data: localStatus } = useRoomLocalStatus(roomId);
  const { data: activeProfile } = useActiveProfile();
  const leaveRoom = useLeaveRoom();
  const prepareRevision = usePrepareRoomRevision();
  const createRoomProfile = useCreateRoomProfile();
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
  const canPrepare = workflow.synchronized && manifest !== null && manifest !== undefined;
  const canCreateProfile = workflow.prepared && manifest !== null && manifest !== undefined;

  function leave() {
    leaveRoom.mutate(roomId, {
      onSuccess: () => toast.success(m.rooms_left_title(), m.rooms_left_description()),
      onError: (error) => toast.error(m.rooms_leave_failed_title(), errorMessage(error)),
    });
  }

  function prepare() {
    prepareRevision.mutate(roomId, {
      onSuccess: (summary) => {
        toast.success(
          m.rooms_prepare_done_title(),
          m.rooms_prepare_done_description({
            imported: summary.importedCount,
            reused: summary.reusedCount,
          }),
        );
      },
      onError: (error) => toast.error(m.rooms_prepare_failed_title(), errorMessage(error)),
    });
  }

  function createProfile() {
    createRoomProfile.mutate(roomId, {
      onSuccess: () =>
        toast.success(m.rooms_profile_done_title(), m.rooms_profile_done_description()),
      onError: (error) => toast.error(m.rooms_profile_failed_title(), errorMessage(error)),
    });
  }

  return (
    <div className="space-y-4" data-ui="RoomWorkspace:detail">
      <SectionCard
        title={m.rooms_workspace_title()}
        description={m.rooms_workspace_draft()}
        icon={<LockKeyIcon className="h-4 w-4" />}
        action={
          <Button
            variant="ghost"
            size="sm"
            left={<SignOutIcon weight="bold" />}
            loading={leaveRoom.isPending}
            onClick={leave}
          >
            {m.rooms_leave_action()}
          </Button>
        }
      >
        <div className="flex flex-wrap items-center justify-between gap-3">
          <div className="min-w-0">
            <Code className="text-sm">{roomId}</Code>
            {membership && (
              <p className="mt-2 text-xs text-surface-400">
                {m.rooms_workspace_member({ memberId: membership.memberId })}
              </p>
            )}
          </div>
          <div className="rounded-lg bg-surface-800 px-3 py-2 text-right">
            <p className="text-xs text-surface-400">{m.rooms_revision_label()}</p>
            <p className="font-mono text-sm text-surface-100">
              {revisionLabel(snapshot?.activeRevision)}
            </p>
          </div>
        </div>
      </SectionCard>

      <div className="grid gap-4 xl:grid-cols-[minmax(0,1.35fr)_minmax(16rem,0.65fr)]">
        <SectionCard
          title={m.rooms_sync_title()}
          description={m.rooms_sync_description()}
          icon={<CloudArrowDownIcon className="h-4 w-4" />}
        >
          <div className="flex items-center gap-2 text-sm text-surface-200">
            <span
              className={
                workflow.synchronized
                  ? "h-2 w-2 rounded-full bg-success"
                  : "h-2 w-2 rounded-full bg-surface-500"
              }
            />
            {phaseLabel(snapshot?.phase)}
          </div>
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
          {!manifest && <p className="text-sm text-surface-400">{m.rooms_sync_no_manifest()}</p>}
        </SectionCard>

        <SectionCard
          title={m.rooms_members_title()}
          description={m.rooms_members_description()}
          icon={<UsersThreeIcon className="h-4 w-4" />}
        >
          {membership && (
            <div className="flex items-center gap-3 rounded-lg border border-surface-700/60 bg-surface-800/35 p-3">
              <CheckCircleIcon weight="fill" className="h-5 w-5 text-surface-400" />
              <div className="min-w-0">
                <p className="text-sm font-medium text-surface-100">{m.rooms_member_you()}</p>
                <p className="truncate text-xs text-surface-400">{m.rooms_member_local_draft()}</p>
              </div>
            </div>
          )}
          <p className="text-sm text-surface-400">{m.rooms_member_remote_pending()}</p>
        </SectionCard>
      </div>

      <SectionCard
        title={m.rooms_manifest_title()}
        description={m.rooms_manifest_description()}
        icon={<FolderSimpleIcon className="h-4 w-4" />}
      >
        {!manifest && (
          <EmptyState
            size="sm"
            icon={<FolderSimpleIcon className="h-12 w-12" />}
            title={m.rooms_manifest_empty_title()}
            description={m.rooms_manifest_empty_description()}
          />
        )}
        {manifest && (
          <div className="overflow-x-auto rounded-lg border border-surface-700/60">
            <table className="w-full min-w-[34rem] text-left text-sm">
              <thead className="bg-surface-800 text-xs text-surface-400">
                <tr>
                  <th className="px-3 py-2 font-medium">{m.rooms_file_name()}</th>
                  <th className="px-3 py-2 font-medium">{m.rooms_file_format()}</th>
                  <th className="px-3 py-2 text-right font-medium">{m.rooms_file_size()}</th>
                </tr>
              </thead>
              <tbody className="divide-y divide-surface-700/60">
                {manifest.mods.map((mod) => (
                  <tr key={mod.contentHash}>
                    <td className="px-3 py-2 text-surface-100">
                      <div>{mod.displayName}</div>
                      {mod.version && <div className="text-xs text-surface-400">{mod.version}</div>}
                    </td>
                    <td className="px-3 py-2 font-mono text-xs text-surface-300">{mod.format}</td>
                    <td className="px-3 py-2 text-right text-surface-300">
                      {formatBytes(Number(mod.sizeBytes))}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </SectionCard>

      <SectionCard
        title={m.rooms_workflow_title()}
        description={m.rooms_workflow_description()}
        icon={<GameControllerIcon className="h-4 w-4" />}
      >
        <WorkflowStep title={m.rooms_step_synchronized_title()} ready={workflow.synchronized}>
          {workflow.synchronized &&
            m.rooms_step_synchronized_ready({ revision: snapshot?.activeRevision ?? 0 })}
          {!workflow.synchronized && m.rooms_step_synchronized_waiting()}
        </WorkflowStep>
        <WorkflowStep title={m.rooms_step_prepared_title()} ready={workflow.prepared}>
          {workflow.prepared &&
            m.rooms_step_prepared_ready({ revision: localStatus?.preparedRevision ?? 0 })}
          {!workflow.prepared && <p>{m.rooms_step_prepared_waiting()}</p>}
          {workflow.synchronized && !workflow.prepared && (
            <div className="mt-3 flex flex-wrap items-center gap-2">
              <Button
                size="sm"
                variant="duotone"
                left={<CloudArrowDownIcon weight="bold" />}
                onClick={prepare}
                loading={prepareRevision.isPending}
                disabled={!canPrepare}
              >
                {m.rooms_prepare_action()}
              </Button>
              <span className="text-xs text-surface-400">{m.rooms_prepare_description()}</span>
            </div>
          )}
          {!workflow.synchronized && (
            <p className="mt-2 text-xs text-surface-500">{m.rooms_workflow_unavailable()}</p>
          )}
        </WorkflowStep>
        <WorkflowStep title={m.rooms_step_profile_title()} ready={workflow.profileReady}>
          {workflow.profileReady &&
            m.rooms_step_profile_ready({ revision: localStatus?.profile?.revision ?? 0 })}
          {!workflow.profileReady && <p>{m.rooms_step_profile_waiting()}</p>}
          {workflow.prepared && !workflow.profileReady && (
            <div className="mt-3 flex flex-wrap items-center gap-2">
              <Button
                size="sm"
                variant="duotone"
                left={<FolderSimpleIcon weight="bold" />}
                onClick={createProfile}
                loading={createRoomProfile.isPending}
                disabled={!canCreateProfile}
              >
                {m.rooms_profile_action()}
              </Button>
              <span className="text-xs text-surface-400">{m.rooms_profile_description()}</span>
            </div>
          )}
        </WorkflowStep>
        <WorkflowStep title={m.rooms_step_applied_title()} ready={false}>
          {workflow.profileSelected && <p>{m.rooms_step_applied_selected()}</p>}
          {!workflow.profileSelected && <p>{m.rooms_step_applied_waiting()}</p>}
          {workflow.profileReady && (
            <div className="mt-3 flex flex-wrap items-center gap-2">
              <Button
                size="sm"
                variant="outline"
                left={<ArrowRightIcon weight="bold" />}
                onClick={() => navigate({ to: "/mods" })}
              >
                {m.rooms_open_mods_action()}
              </Button>
              {activeProfile && (
                <span className="text-xs text-surface-400">
                  {m.rooms_active_profile_label()}: {activeProfile.name}
                </span>
              )}
            </div>
          )}
        </WorkflowStep>
      </SectionCard>
    </div>
  );
}
