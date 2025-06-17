use hickory_resolver::proto::op::{Message, MessageType, OpCode, ResponseCode};
use hickory_resolver::proto::rr::DNSClass;
use log::warn;
use std::future::Future;
use std::io;
use std::task::Poll;

pub struct MessageProcessor {
    req_message: Message,
}

impl MessageProcessor {
    pub fn new(req_message: Message) -> Self {
        Self { req_message }
    }
}

impl Future for MessageProcessor {
    type Output = io::Result<Message>;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> Poll<Self::Output> {
        let mut resp_message = Message::new();
        resp_message.set_header(self.req_message.header().clone());
        // Only accept the oper code is query or message type is query
        if self.req_message.op_code() != OpCode::Query
            || self.req_message.message_type() != MessageType::Query
        {
            resp_message.set_response_code(ResponseCode::NotImp);
        } else {
            for query in self.req_message.queries() {
                resp_message.add_query(query.clone());
                if query.query_class() != DNSClass::IN {
                    warn!(
                        "Query class {:?} is not support. Full Message: {:?}",
                        query.query_class(),
                        self.req_message
                    );
                    continue;
                }
            }
        }
        return Poll::Ready(Ok(resp_message));
    }
}
