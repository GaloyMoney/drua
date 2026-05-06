ALTER TABLE public.sandboxes
    ADD COLUMN deleted boolean DEFAULT false NOT NULL;
