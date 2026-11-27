import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import type { ReactNode } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const invoke = vi.hoisted(() => vi.fn());

vi.mock("@tauri-apps/api/core", () => ({ invoke }));
vi.mock("react-i18next", () => ({
  useTranslation: () => ({
    t: (key: string, options?: { detail?: string }) =>
      options?.detail ? `${key}: ${options.detail}` : key,
  }),
}));
vi.mock("../../elements/Modal", () => ({
  Modal: ({ children }: { children: ReactNode }) => <>{children}</>,
}));

import ScreenSharePickerDialog from "./ScreenSharePickerDialog";

const SCREEN = {
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
  invoke.mockImplementation(async (command: string, args?: { includeDevices?: boolean }) => {
    if (command === "list_capture_sources") {
      return args?.includeDevices ? [SCREEN, CAMERA] : [SCREEN];
    }
    if (command === "request_camera_access") return true;
    if (command === "capture_source_thumbnail") throw new Error("no native capture in jsdom");
    throw new Error(`unexpected Tauri command: ${command}`);
  });
});

afterEach(() => {
  cleanup();
});

describe("ScreenSharePickerDialog camera access boundary", () => {
  it("lists screens without touching AVFoundation when the picker opens", async () => {
    render(<ScreenSharePickerDialog onConfirm={vi.fn()} onCancel={vi.fn()} />);

    await waitFor(() => {
      expect(invoke).toHaveBeenCalledWith("list_capture_sources");
    });
    expect(invoke).not.toHaveBeenCalledWith("request_camera_access");
    expect(
      invoke.mock.calls.some(
        ([command, args]) => command === "list_capture_sources" && args?.includeDevices === true,
      ),
    ).toBe(false);
  });

  it("requests camera access before asking the native layer to enumerate devices", async () => {
    render(<ScreenSharePickerDialog onConfirm={vi.fn()} onCancel={vi.fn()} />);

    await waitFor(() => expect(invoke).toHaveBeenCalledWith("list_capture_sources"));

    const devicesTab = document.querySelector<HTMLButtonElement>('button[data-tab="devices"]');
    expect(devicesTab).not.toBeNull();
    fireEvent.click(devicesTab!);

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
