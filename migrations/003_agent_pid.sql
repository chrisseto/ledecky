-- Process id of the card's agent, so a server that died without running its
-- shutdown hook can sweep up whatever it left behind.
ALTER TABLE cards ADD COLUMN agent_pid INTEGER;
