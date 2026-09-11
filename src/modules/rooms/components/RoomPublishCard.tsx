import { PaperPlaneTiltIcon } from "@phosphor-icons/react";
import { useEffect, useState } from "react";

import { Button, SectionCard, SelectField, useToast } from "@/components";
import { errorMessage, m } from "@/i18n";
import { useActiveProfile, useProfiles } from "@/modules/library";

import { usePublishRoomProfile, useRemoteRoomMembers, useRoomMemberships } from "../api";

/** Lets the room owner send one of their profiles to the room as its next manifest revision. */
export function RoomPublishCard({ roomId }: { roomId: string }) {
  const toast = useToast();
  const { data: rooms = [] } = useRoomMemberships();
  const { data: members = [] } = useRemoteRoomMembers(roomId);
  const { data: profiles = [] } = useProfiles();
  const { data: activeProfile } = useActiveProfile();
  const publishProfile = usePublishRoomProfile();
  const [profileId, setProfileId] = useState("");

  const membership = rooms.find((room) => room.roomId === roomId);
  const isOwner = members.some(
    (member) => member.memberId === membership?.memberId && member.role === "owner",
  );

  useEffect(() => {
    if (profileId || !activeProfile) return;
    setProfileId(activeProfile.id);
  }, [activeProfile, profileId]);

  if (!isOwner) return null;

  function publish() {
    if (!profileId) return;
    publishProfile.mutate(
      { roomId, profileId },
      {
        onSuccess: () =>
          toast.success(m.rooms_publish_done_title(), m.rooms_publish_done_description()),
        onError: (error) => toast.error(m.rooms_publish_failed_title(), errorMessage(error)),
      },
    );
  }

  return (
    <SectionCard
      title={m.rooms_publish_title()}
      description={m.rooms_publish_description()}
      icon={<PaperPlaneTiltIcon className="h-4 w-4" />}
    >
      <div className="flex flex-wrap items-end gap-3">
        <SelectField
          className="min-w-56"
          label={m.rooms_publish_profile_label()}
          options={profiles.map((profile) => ({ value: profile.id, label: profile.name }))}
          value={profileId}
          onValueChange={(value) => setProfileId(value ?? "")}
        />
        <Button
          variant="filled"
          left={<PaperPlaneTiltIcon weight="bold" />}
          onClick={publish}
          loading={publishProfile.isPending}
          disabled={!profileId}
        >
          {m.rooms_publish_action()}
        </Button>
      </div>
    </SectionCard>
  );
}
