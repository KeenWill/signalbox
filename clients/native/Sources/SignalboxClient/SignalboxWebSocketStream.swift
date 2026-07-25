import Foundation
#if canImport(SignalboxModels)
import SignalboxModels
#endif

public enum SignalboxWebSocketMessage: Equatable, Sendable {
    case data(Data)
    case string(String)
}

public protocol SignalboxWebSocketTransport: Sendable {
    func receive() async throws -> SignalboxWebSocketMessage
    func send(_ message: SignalboxWebSocketMessage) async throws
    func cancel() async
}

public struct URLSessionSignalboxWebSocketTransport: SignalboxWebSocketTransport, @unchecked Sendable {
    private let task: URLSessionWebSocketTask

    public init(url: URL, session: URLSession = .shared) {
        self.task = session.webSocketTask(with: url)
        self.task.resume()
    }

    public func receive() async throws -> SignalboxWebSocketMessage {
        switch try await task.receive() {
        case .data(let data):
            return .data(data)
        case .string(let string):
            return .string(string)
        @unknown default:
            throw SignalboxWebSocketStreamError.unsupportedMessage
        }
    }

    public func send(_ message: SignalboxWebSocketMessage) async throws {
        switch message {
        case .data(let data):
            try await task.send(.data(data))
        case .string(let string):
            try await task.send(.string(string))
        }
    }

    public func cancel() async {
        task.cancel(with: .goingAway, reason: nil)
    }
}

public enum SignalboxWebSocketStreamError: LocalizedError, Equatable {
    case connectionWentQuiet
    case unsupportedMessage

    public var errorDescription: String? {
        switch self {
        case .connectionWentQuiet:
            return "The server connection stopped receiving heartbeats."
        case .unsupportedMessage:
            return "The server sent an unsupported WebSocket message."
        }
    }
}

public final class SignalboxWebSocketStream: Sendable {
    public static let defaultHeartbeatTimeout: Duration = .seconds(45)

    private let transport: any SignalboxWebSocketTransport
    private let heartbeatTimeout: Duration

    public convenience init(
        url: URL,
        heartbeatTimeout: Duration = SignalboxWebSocketStream.defaultHeartbeatTimeout
    ) {
        self.init(
            transport: URLSessionSignalboxWebSocketTransport(url: url),
            heartbeatTimeout: heartbeatTimeout
        )
    }

    public init(
        transport: any SignalboxWebSocketTransport,
        heartbeatTimeout: Duration = SignalboxWebSocketStream.defaultHeartbeatTimeout
    ) {
        self.transport = transport
        self.heartbeatTimeout = heartbeatTimeout
    }

    public func messages() -> AsyncThrowingStream<SignalboxServerMessage, Error> {
        AsyncThrowingStream { continuation in
            let watchdog = SignalboxHeartbeatWatchdog(timeout: heartbeatTimeout)
            let timeoutAction: @Sendable () async -> Void = {
                continuation.finish(throwing: SignalboxWebSocketStreamError.connectionWentQuiet)
                await self.transport.cancel()
            }
            let receiveTask = Task {
                await watchdog.arm(onTimeout: timeoutAction)
                do {
                    while !Task.isCancelled {
                        let message = try await transport.receive()
                        let data: Data
                        switch message {
                        case .data(let receivedData):
                            data = receivedData
                        case .string(let string):
                            data = Data(string.utf8)
                        }
                        let decoded: SignalboxServerMessage
                        do {
                            decoded = try SignalboxJSONCoding.decoder().decode(SignalboxServerMessage.self, from: data)
                        } catch {
                            continuation.yield(.diagnostic(SignalboxDecodingDiagnostic(error: error)))
                            continue
                        }
                        if case .heartbeat(let sentAt) = decoded {
                            try await sendHeartbeatAck(sentAt: sentAt)
                            await watchdog.arm(onTimeout: timeoutAction)
                            continue
                        }
                        continuation.yield(decoded)
                    }
                } catch {
                    await watchdog.cancel()
                    continuation.finish(throwing: error)
                }
            }

            continuation.onTermination = { _ in
                receiveTask.cancel()
                Task {
                    await watchdog.cancel()
                    await self.transport.cancel()
                }
            }
        }
    }

    private func sendHeartbeatAck(sentAt: Date) async throws {
        let payload = SignalboxHeartbeatAck(kind: "heartbeat_ack", sentAt: sentAt)
        let data = try SignalboxJSONCoding.encoder().encode(payload)
        guard let string = String(data: data, encoding: .utf8) else {
            return
        }
        try await transport.send(.string(string))
    }
}

private actor SignalboxHeartbeatWatchdog {
    private let timeout: Duration
    private var timeoutTask: Task<Void, Never>?

    init(timeout: Duration) {
        self.timeout = timeout
    }

    func arm(onTimeout: @escaping @Sendable () async -> Void) {
        timeoutTask?.cancel()
        timeoutTask = Task {
            do {
                try await Task.sleep(for: timeout)
            } catch {
                return
            }
            await onTimeout()
        }
    }

    func cancel() {
        timeoutTask?.cancel()
        timeoutTask = nil
    }
}

private struct SignalboxHeartbeatAck: Encodable {
    let kind: String
    let sentAt: Date

    private enum CodingKeys: String, CodingKey {
        case kind
        case sentAt = "sent_at"
    }
}
