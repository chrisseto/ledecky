-- `awaiting_permission` covered every dialog, not just permission prompts.
-- `AgentState::parse` falls back to `stopped`, so leaving the old spelling in
-- place would quietly reset any card holding it.
UPDATE cards SET agent_state = 'awaiting_user' WHERE agent_state = 'awaiting_permission';
