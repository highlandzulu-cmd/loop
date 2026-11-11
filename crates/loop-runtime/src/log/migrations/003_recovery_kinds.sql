-- Recovery reads a few kinds per generation.
CREATE INDEX events_generation_kind ON events(generation, kind);
