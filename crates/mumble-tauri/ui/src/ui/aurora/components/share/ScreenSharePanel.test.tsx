import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import type { ReactNode } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const invoke = vi.hoisted(() => vi.fn());
const storeState = vi.hoisted(() => ({
  isSharingOwn: false,
  broadcastingSessions: new Set<number>(),
  users: [] as Array<{ session: number; name: string }>,
}));

vi.mock("@tauri-apps/api/core", () => ({ invoke }));
vi.mock("@core/store", () => ({
  useAppStore: Object.assign(
    (selector: (state: typeof storeState) => unknown) => selector(storeState),
    {
      getState: () => storeState,
      setState: vi.fn(),
    },
  ),
}));
vi.mock("../primitives", () => ({
  Button: ({
    children,
    onClick,
    disabled,
    role,
    ...props
  }: {
    children: ReactNode;
    onClick?: () => void;
    disabled?: boolean;
    role?: string;
  }) => (
    <button onClick={onClick} disabled={disabled} role={role} {...props}>
      {children}
    </button>
  ),
  ModalSurface: ({ children, title }: { children: ReactNode; title: string }) => (
    <div role="dialog">
      <h1>{title}</h1>
      {children}
    </div>
  ),
}));

import ScreenSharePanel from "./ScreenSharePanel";

const DISPLAY = {
  id: 100,
  kind: "screen",
  title: "Screen 1",
  width: 1920,
  height: 1080,
} as const;
const CAMERA = {
  id: 0,
  kind: "device",
  title: "USB camera",
  width: 0,
  height: 0,
} as const;

beforeEach(() => {
  invoke.mockReset();
  invoke.mockImplementation(
    async (command: string, args?: { includeDevices?: boolean }) => {
      if (command === "request_screen_capture_access") return true;
      if (command === "request_camera_access") return true;
      if (command === "list_capture_sources") {
        return args?.includeDevices ? [DISPLAY, CAMERA] : [DISPLAY];
      }
      if (command === "capture_source_thumbnail") {
        throw new Error("native capture is not available in jsdom");
      }
      throw new Error(`unexpected Tauri command: ${command}`);
    },
  );
});

afterEach(() => cleanup());

describe("Aurora ScreenSharePanel permissions", () => {
  it("requests screen permission before enumerating desktop sources", async () => {
    render(<ScreenSharePanel onClose={vi.fn()} />);

    await waitFor(() => {
      expect(invoke).toHaveBeenCalledWith("list_capture_sources");
    });
    const calls = invoke.mock.calls.map(([command]) => command);
    expect(calls.indexOf("request_screen_capture_access")).toBeGreaterThanOrEqual(0);
    expect(calls.indexOf("list_capture_sources")).toBeGreaterThan(
      calls.indexOf("request_screen_capture_access"),
    );
    expect(invoke).not.toHaveBeenCalledWith("request_camera_access");
  });

  it("does not enumerate desktop sources when screen permission is denied", async () => {
    invoke.mockImplementation(async (command: string) => {
      if (command === "request_screen_capture_access") return false;
      throw new Error(`unexpected command after permission denial: ${command}`);
    });

    render(<ScreenSharePanel onClose={vi.fn()} />);

    await waitFor(() => {
      expect(screen.getByText(/Screen Recording permission is required/)).toBeTruthy();
    });
    expect(invoke).not.toHaveBeenCalledWith("list_capture_sources");
    expect(invoke).not.toHaveBeenCalledWith("capture_source_thumbnail");
  });

  it("requests camera access before enumerating camera devices", async () => {
    render(<ScreenSharePanel onClose={vi.fn()} />);

    await waitFor(() => expect(invoke).toHaveBeenCalledWith("list_capture_sources"));
    fireEvent.click(screen.getByRole("tab", { name: /Cameras/i }));

    await waitFor(() => {
      expect(invoke).toHaveBeenCalledWith("list_capture_sources", { includeDevices: true });
    });
    const calls = invoke.mock.calls.map(([command, args]) => [command, args] as const);
    const permissionIndex = calls.findIndex(([command]) => command === "request_camera_access");
    const deviceQueryIndex = calls.findIndex(
      ([command, args]) => command === "list_capture_sources" && args?.includeDevices === true,
    );
    expect(permissionIndex).toBeGreaterThanOrEqual(0);
    expect(deviceQueryIndex).toBeGreaterThan(permissionIndex);
    expect(screen.getByText("USB camera")).toBeTruthy();
  });
});
