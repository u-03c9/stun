#[cfg(test)]
mod agent_test;

use crate::errors::*;
use crate::message::*;

use util::Error;

use tokio::sync::mpsc;
use tokio::time::Instant;

use std::collections::HashMap;
use std::sync::Arc;

use crate::client::ClientTransaction;
use rand::Rng;

// Handler handles state changes of transaction.
// Handler is called on transaction state change.
// Usage of e is valid only during call, user must
// copy needed fields explicitly.
pub type Handler = Option<Arc<mpsc::UnboundedSender<Event>>>;

// noop_handler just discards any event.
pub fn noop_handler() -> Handler {
    None
}

// Agent is low-level abstraction over transaction list that
// handles concurrency (all calls are goroutine-safe) and
// time outs (via Collect call).
pub struct Agent {
    // transactions is map of transactions that are currently
    // in progress. Event handling is done in such way when
    // transaction is unregistered before AgentTransaction access,
    // minimizing mux lock and protecting AgentTransaction from
    // data races via unexpected concurrent access.
    transactions: HashMap<TransactionId, AgentTransaction>,
    closed: bool,     // all calls are invalid if true
    handler: Handler, // handles transactions
}

#[derive(Debug, Clone)]
pub enum EventType {
    Callback(TransactionId),
    Insert(ClientTransaction),
    Remove(TransactionId),
    Close,
}

impl Default for EventType {
    fn default() -> Self {
        EventType::Callback(TransactionId::default())
    }
}

// Event is passed to Handler describing the transaction event.
// Do not reuse outside Handler.
#[derive(Debug, Clone)]
pub struct Event {
    pub event_type: EventType,
    pub event_body: Result<Message, Error>,
}

impl Default for Event {
    fn default() -> Self {
        Event {
            event_type: EventType::default(),
            event_body: Ok(Message::default()),
        }
    }
}

// AgentTransaction represents transaction in progress.
// Concurrent access is invalid.
pub(crate) struct AgentTransaction {
    id: TransactionId,
    deadline: Instant,
}

// AGENT_COLLECT_CAP is initial capacity for Agent.Collect slices,
// sufficient to make function zero-alloc in most cases.
const AGENT_COLLECT_CAP: usize = 100;

#[derive(PartialEq, Eq, Hash, Copy, Clone, Default, Debug)]
pub struct TransactionId(pub [u8; TRANSACTION_ID_SIZE]);

impl TransactionId {
    // NewTransactionID returns new random transaction ID using crypto/rand
    // as source.
    pub fn new() -> Self {
        let mut b = TransactionId([0u8; TRANSACTION_ID_SIZE]);
        rand::thread_rng().fill(&mut b.0);
        b
    }
}

impl Setter for TransactionId {
    fn add_to(&self, m: &mut Message) -> Result<(), Error> {
        m.transaction_id = *self;
        m.write_transaction_id();
        Ok(())
    }
}

// ClientAgent is Agent implementation that is used by Client to
// process transactions.
pub enum ClientAgent {
    Process(Message),
    Collect(Instant),
    Start(TransactionId, Instant),
    Stop(TransactionId),
    Close,
}

// NewAgent initializes and returns new Agent with provided handler.
// If h is nil, the noop_handler will be used.
impl Agent {
    pub fn new(handler: Handler) -> Self {
        Agent {
            transactions: HashMap::new(),
            closed: false,
            handler,
        }
    }

    // stop_with_error removes transaction from list and calls handler with
    // provided error. Can return ErrTransactionNotExists and ErrAgentClosed.
    fn stop_with_error(&mut self, id: TransactionId, error: Error) -> Result<(), Error> {
        if self.closed {
            return Err(ERR_AGENT_CLOSED.clone());
        }

        let v = self.transactions.remove(&id);
        if let Some(t) = v {
            if let Some(handler) = &self.handler {
                handler.send(Event {
                    event_type: EventType::Callback(t.id),
                    event_body: Err(error),
                })?;
            }
            Ok(())
        } else {
            Err(ERR_TRANSACTION_NOT_EXISTS.clone())
        }
    }

    // process incoming message, synchronously passing it to handler.
    fn process(&mut self, message: Message) -> Result<(), Error> {
        if self.closed {
            return Err(ERR_AGENT_CLOSED.clone());
        }

        self.transactions.remove(&message.transaction_id);

        let e = Event {
            event_type: EventType::Callback(message.transaction_id),
            event_body: Ok(message),
        };

        if let Some(handler) = &self.handler {
            handler.send(e)?;
        }

        Ok(())
    }

    // Close terminates all transactions with ErrAgentClosed and renders Agent to
    // closed state.
    fn close(&mut self) -> Result<(), Error> {
        if self.closed {
            return Err(ERR_AGENT_CLOSED.clone());
        }

        for id in self.transactions.keys() {
            let e = Event {
                event_type: EventType::Callback(*id),
                event_body: Err(ERR_AGENT_CLOSED.clone()),
            };
            if let Some(handler) = &self.handler {
                handler.send(e)?;
            }
        }
        self.transactions = HashMap::new();
        self.closed = true;
        self.handler = noop_handler();

        Ok(())
    }

    // Start registers transaction with provided id and deadline.
    // Could return ErrAgentClosed, ErrTransactionExists.
    //
    // Agent handler is guaranteed to be eventually called.
    fn start(&mut self, id: TransactionId, deadline: Instant) -> Result<(), Error> {
        if self.closed {
            return Err(ERR_AGENT_CLOSED.clone());
        }
        if self.transactions.contains_key(&id) {
            return Err(ERR_TRANSACTION_EXISTS.clone());
        }

        self.transactions
            .insert(id, AgentTransaction { id, deadline });

        Ok(())
    }

    // Stop stops transaction by id with ErrTransactionStopped, blocking
    // until handler returns.
    fn stop(&mut self, id: TransactionId) -> Result<(), Error> {
        self.stop_with_error(id, ERR_TRANSACTION_STOPPED.clone())
    }

    // Collect terminates all transactions that have deadline before provided
    // time, blocking until all handlers will process ErrTransactionTimeOut.
    // Will return ErrAgentClosed if agent is already closed.
    //
    // It is safe to call Collect concurrently but makes no sense.
    fn collect(&mut self, deadline: Instant) -> Result<(), Error> {
        if self.closed {
            // Doing nothing if agent is closed.
            // All transactions should be already closed
            // during Close() call.
            return Err(ERR_AGENT_CLOSED.clone());
        }

        let mut to_remove: Vec<TransactionId> = Vec::with_capacity(AGENT_COLLECT_CAP);

        // Adding all transactions with deadline before gc_time
        // to toCall and to_remove slices.
        // No allocs if there are less than AGENT_COLLECT_CAP
        // timed out transactions.
        for (id, t) in &self.transactions {
            if t.deadline < deadline {
                to_remove.push(*id);
            }
        }
        // Un-registering timed out transactions.
        for id in &to_remove {
            self.transactions.remove(id);
        }

        for id in to_remove {
            let event = Event {
                event_type: EventType::Callback(id),
                event_body: Err(ERR_TRANSACTION_TIME_OUT.clone()),
            };
            if let Some(handler) = &self.handler {
                handler.send(event)?;
            }
        }

        Ok(())
    }

    // set_handler sets agent handler to h.
    fn set_handler(&mut self, h: Handler) -> Result<(), Error> {
        if self.closed {
            return Err(ERR_AGENT_CLOSED.clone());
        }
        self.handler = h;

        Ok(())
    }

    pub async fn run(mut agent: Agent, mut rx: mpsc::Receiver<ClientAgent>) {
        while let Some(client_agent) = rx.recv().await {
            let result = match client_agent {
                ClientAgent::Process(message) => agent.process(message),
                ClientAgent::Collect(deadline) => agent.collect(deadline),
                ClientAgent::Start(tid, deadline) => agent.start(tid, deadline),
                ClientAgent::Stop(tid) => agent.stop(tid),
                ClientAgent::Close => agent.close(),
            };

            if let Err(err) = result {
                if err == *ERR_AGENT_CLOSED {
                    break;
                }
            }
        }
    }
}
