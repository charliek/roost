-- 0002_user_titled: lock manual tab renames against OSC 1/2 overwrites.
--
-- Set to 1 by Workspace.RenameTab (Cmd-R popover, IPC tab.set_title).
-- SetTabTitleFromOSC's UPDATE filters on user_titled = 0 so OSC writes
-- to a locked tab no-op atomically.

ALTER TABLE tab ADD COLUMN user_titled INTEGER NOT NULL DEFAULT 0;
