import Foundation
#if canImport(LLMHubModels)
import LLMHubModels
#endif

public final class HubWebSocketStream: Sendable {
    private let url: URL

    public init(url: URL) {
        self.url = url
    }

    public func messages() -> AsyncThrowingStream<HubServerMessage, Error> {
        AsyncThrowingStream { continuation in
            let task = URLSession.shared.webSocketTask(with: url)
            task.resume()

            let receiveTask = Task {
                do {
                    while !Task.isCancelled {
                        let message = try await task.receive()
                        let data: Data
                        switch message {
                        case .data(let receivedData):
                            data = receivedData
                        case .string(let string):
                            data = Data(string.utf8)
                        @unknown default:
                            continue
                        }
                        let decoded = try HubJSONCoding.decoder().decode(HubServerMessage.self, from: data)
                        if case .heartbeat(let sentAt) = decoded {
                            try await sendHeartbeatAck(sentAt: sentAt, task: task)
                            continue
                        }
                        continuation.yield(decoded)
                    }
                } catch {
                    continuation.finish(throwing: error)
                }
            }

            continuation.onTermination = { _ in
                receiveTask.cancel()
                task.cancel(with: .goingAway, reason: nil)
            }
        }
    }

    private func sendHeartbeatAck(sentAt: Date, task: URLSessionWebSocketTask) async throws {
        let payload = HubHeartbeatAck(kind: "heartbeat_ack", sentAt: sentAt)
        let data = try HubJSONCoding.encoder().encode(payload)
        guard let string = String(data: data, encoding: .utf8) else {
            return
        }
        try await task.send(.string(string))
    }
}

private struct HubHeartbeatAck: Encodable {
    let kind: String
    let sentAt: Date

    private enum CodingKeys: String, CodingKey {
        case kind
        case sentAt = "sent_at"
    }
}
