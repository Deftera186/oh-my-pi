# omp-journal

`omp-journal` owns the durable, crash-tolerant history of an omp session. A session is one flat `.oms` file containing raw Server-Sent Events frames, plus content-addressed blobs for payloads that should not be repeated inline.

The crate is intentionally structural rather than behavioral. It assigns monotonic identities, commits complete frames, recovers a torn tail, and selects a branch through `prior` links. It does not interpret session state: replay and materialization belong to the DOM/session layer, so the journal remains the sole durable authority without becoming a second state model.
