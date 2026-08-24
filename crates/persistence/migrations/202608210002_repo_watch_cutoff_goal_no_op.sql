-- Record repository-watch cutoff sessions that needed no stop.
--
-- A convergence cutoff withdraws every still-active generation-one goal
-- commissioned for its pull request. A session that has already terminated, or
-- that has moved beyond the generation the cutoff addressed, needs no stop and
-- receives no goal command. Before this change such a session was recorded
-- nowhere, so it stayed eligible for the cutoff forever: the reprocessing
-- selection kept returning it, the enclosing candidate query kept returning the
-- same assessment, and the repository worker wedged re-running that cutoff.
--
-- The cutoff-goal row becomes the durable disposition of every session a cutoff
-- considered, and its goal command becomes optional: present when a stop was
-- issued, absent when the session needed none. The composite foreign key to
-- goal_command is MATCH SIMPLE, so a null command leaves it satisfied without
-- naming a command that was never written, and UNIQUE admits many nulls.

ALTER TABLE repo_watch_convergence_cutoff_goal
    ALTER COLUMN goal_command_id DROP NOT NULL;
