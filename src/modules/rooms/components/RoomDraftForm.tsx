import { SignInIcon, UsersThreeIcon } from "@phosphor-icons/react";
import { useEffect, useState } from "react";

import { Button, FormField, SectionCard, SelectField, useToast } from "@/components";
import { errorMessage, errorTitle, m } from "@/i18n";
import type { JoinedRoom } from "@/lib/bindings.gen";
import { useActiveProfile, useProfiles } from "@/modules/library";

import { useCreateRemoteRoom, useJoinRemoteRoom } from "../api";

type Mode = "create" | "join";

export function RoomDraftForm({ onOpened }: { onOpened: (roomId: string) => void }) {
  const [mode, setMode] = useState<Mode>("join");
  const [roomCode, setRoomCode] = useState("");
  const [password, setPassword] = useState("");
  const [profileId, setProfileId] = useState("");
  const { data: profiles = [] } = useProfiles();
  const { data: activeProfile } = useActiveProfile();
  const toast = useToast();
  const createRemote = useCreateRemoteRoom();
  const joinRemote = useJoinRemoteRoom();
  const busy = createRemote.isPending || joinRemote.isPending;

  useEffect(() => {
    if (!profileId && activeProfile) setProfileId(activeProfile.id);
  }, [activeProfile, profileId]);

  function credentials() {
    const roomId = roomCode.trim().toLowerCase();
    if (!roomId) {
      toast.warning(m.rooms_code_required());
      return null;
    }
    if (!/^[a-z0-9_-]{3,64}$/.test(roomId)) {
      toast.warning(
        m["error.ROOM_SYNC.INVALID_ROOM_ID.title"](),
        m["error.ROOM_SYNC.INVALID_ROOM_ID.description"](),
      );
      return null;
    }
    if (!password) {
      toast.warning(m.rooms_password_required());
      return null;
    }
    if (password.length < 4) {
      toast.warning(
        m["error.ROOM_SYNC.PASSWORD_TOO_SHORT.title"](),
        m["error.ROOM_SYNC.PASSWORD_TOO_SHORT.description"](),
      );
      return null;
    }
    return { roomId, password };
  }

  function submit() {
    const values = credentials();
    if (!values) return;
    const callbacks = {
      onSuccess: (room: JoinedRoom) => {
        onOpened(room.roomId);
        setPassword("");
        toast.success(
          mode === "create" ? m.rooms_created_title() : m.rooms_opened_title(),
          m.rooms_draft_ready_description(),
        );
      },
      onError: (error: unknown) =>
        toast.error(errorTitle(error, m.rooms_open_failed_title()), errorMessage(error)),
    };

    if (mode === "create") {
      if (!profileId) {
        toast.warning(m.rooms_profile_required());
        return;
      }
      createRemote.mutate({ ...values, profileId }, callbacks);
    } else {
      joinRemote.mutate(values, callbacks);
    }
  }

  return (
    <SectionCard
      title={m.rooms_join_title()}
      description={m.rooms_join_description()}
      icon={<UsersThreeIcon className="h-4 w-4" />}
    >
      <div className="inline-flex rounded-lg border border-surface-700 bg-surface-900 p-1">
        <Button
          size="sm"
          variant={mode === "join" ? "filled" : "ghost"}
          onClick={() => setMode("join")}
        >
          {m.rooms_join_tab()}
        </Button>
        <Button
          size="sm"
          variant={mode === "create" ? "filled" : "ghost"}
          onClick={() => setMode("create")}
        >
          {m.rooms_create_tab()}
        </Button>
      </div>
      <div className="grid gap-4 md:grid-cols-2">
        <FormField
          label={m.rooms_code_label()}
          description={m.rooms_code_description()}
          placeholder={m.rooms_code_placeholder()}
          value={roomCode}
          onChange={(event) => setRoomCode(event.target.value)}
          autoComplete="off"
        />
        <FormField
          label={m.rooms_password_label()}
          description={m.rooms_password_description()}
          placeholder={m.rooms_password_placeholder()}
          type="password"
          value={password}
          onChange={(event) => setPassword(event.target.value)}
          autoComplete={mode === "join" ? "current-password" : "new-password"}
          onKeyDown={(event) => event.key === "Enter" && submit()}
        />
      </div>
      {mode === "create" && (
        <SelectField
          className="max-w-md"
          label={m.rooms_publish_profile_label()}
          description={m.rooms_profile_description()}
          options={profiles.map((profile) => ({ value: profile.id, label: profile.name }))}
          value={profileId}
          onValueChange={(value) => setProfileId(value ?? "")}
        />
      )}
      <Button
        variant="filled"
        left={mode === "create" ? <UsersThreeIcon weight="bold" /> : <SignInIcon weight="bold" />}
        onClick={submit}
        loading={busy}
        disabled={busy || (mode === "create" && !profileId)}
      >
        {mode === "create" ? m.rooms_create_draft_action() : m.rooms_open_draft_action()}
      </Button>
    </SectionCard>
  );
}
