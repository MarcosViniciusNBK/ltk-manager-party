import { UsersThreeIcon } from "@phosphor-icons/react";
import { Check, Pencil, Trash2, X } from "lucide-react";
import { useEffect, useRef, useState } from "react";

import { Button, Field, IconButton, Tooltip, useToast } from "@/components";
import { m } from "@/i18n";
import type { Profile } from "@/lib/tauri";
import { useRenameProfile } from "@/modules/library/api";

interface ProfileListItemProps {
  profile: Profile;
  isActive: boolean;
  onSwitch: (profileId: string) => void;
  onDeleteClick: (profile: Profile) => void;
  isSwitching: boolean;
}

export function ProfileListItem({
  profile,
  isActive,
  onSwitch,
  onDeleteClick,
  isSwitching,
}: ProfileListItemProps) {
  const renameProfile = useRenameProfile();
  const toast = useToast();

  const [isEditing, setIsEditing] = useState(false);
  const [editName, setEditName] = useState("");
  const inputRef = useRef<HTMLInputElement>(null);

  const isDefaultProfile = profile.name === "Default";

  useEffect(() => {
    if (isEditing) {
      inputRef.current?.focus();
    }
  }, [isEditing]);

  const startEditing = () => {
    setIsEditing(true);
    setEditName(profile.name);
  };

  const cancelEditing = () => {
    setIsEditing(false);
    setEditName("");
  };

  const handleRename = async () => {
    const trimmedName = editName.trim();
    if (!trimmedName || renameProfile.isPending) return;

    try {
      await renameProfile.mutateAsync({ profileId: profile.id, newName: trimmedName });
      setIsEditing(false);
      setEditName("");
      toast.success("Profile renamed");
    } catch {
      /* The default mutation toast reports it. */
    }
  };

  const handleKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === "Enter") {
      handleRename();
    } else if (e.key === "Escape") {
      cancelEditing();
    }
  };

  if (isEditing) {
    return (
      <div className="flex items-center gap-1 p-1">
        <Field.Control
          ref={inputRef}
          type="text"
          value={editName}
          onChange={(e) => setEditName(e.target.value)}
          onKeyDown={handleKeyDown}
          className="h-7 flex-1 px-2 py-1 text-sm"
          placeholder="Profile name..."
        />
        <IconButton
          icon={<Check className="h-4 w-4" />}
          variant="ghost"
          size="xs"
          onClick={handleRename}
          disabled={!editName.trim() || renameProfile.isPending}
          className="text-success-text hover:text-success-text"
        />
        <IconButton
          icon={<X className="h-4 w-4" />}
          variant="ghost"
          size="xs"
          onClick={cancelEditing}
        />
      </div>
    );
  }

  const isRoomPinned = profile.orderMode === "roomPinned";

  return (
    <div className="flex items-center gap-1">
      <Button
        variant="ghost"
        size="sm"
        onClick={() => onSwitch(profile.id)}
        disabled={isSwitching || isActive}
        className="flex-1 justify-between"
        right={isActive ? <Check className="h-4 w-4 text-accent-500" /> : undefined}
      >
        <span className="flex min-w-0 items-center gap-1.5">
          <span className="truncate">{profile.name}</span>
          {isRoomPinned && (
            <Tooltip content={m.library_profile_room_pinned_hint()}>
              <span className="bg-primary-500/20 text-primary-300 inline-flex shrink-0 items-center gap-1 rounded px-1.5 py-0.5 text-[10px] font-semibold">
                <UsersThreeIcon className="h-3 w-3" weight="bold" />
                {m.library_profile_room_pinned_badge()}
              </span>
            </Tooltip>
          )}
        </span>
      </Button>

      {!isDefaultProfile && (
        <>
          <Tooltip content="Rename profile">
            <IconButton
              icon={<Pencil className="h-3.5 w-3.5" />}
              variant="ghost"
              size="xs"
              onClick={startEditing}
            />
          </Tooltip>
          <Tooltip content="Delete profile">
            <IconButton
              icon={<Trash2 className="h-3.5 w-3.5" />}
              variant="ghost"
              size="xs"
              onClick={() => onDeleteClick(profile)}
              disabled={isActive}
              className="hover:text-danger-text"
            />
          </Tooltip>
        </>
      )}
    </div>
  );
}
