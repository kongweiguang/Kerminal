// @author kongweiguang

import { fireEvent, render, screen } from "@testing-library/react";
import userEvent, { type UserEvent } from "@testing-library/user-event";
import { waitFor } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import {
  sftpApiMocks,
  sshMachine,
} from "../../support/sftp/SftpToolContent.testSupport.tsx";
import { SftpToolContent } from "../../../../src/features/sftp/SftpToolContent";

/** 通过用户可见菜单切换视图，避免耦合工具栏的内部按钮结构。 */
async function chooseMode(
  user: UserEvent,
  name: "列表模式" | "树形模式" | "工作区模式",
) {
  await user.click(screen.getByRole("button", { name: "切换文件视图" }));
  await user.click(screen.getByRole("menuitemradio", { name }));
}

describe("SftpToolContent browser modes", () => {
  it("switches between list, tree, and workspace through the view menu", async () => {
    const user = userEvent.setup();
    render(<SftpToolContent selectedMachine={sshMachine} />);

    await screen.findByTestId("sftp-remote-entry-list");
    await chooseMode(user, "树形模式");
    expect(screen.queryByTestId("sftp-remote-entry-list")).not.toBeInTheDocument();
    expect(screen.getByRole("tree", { name: "SFTP 目录树" })).toBeInTheDocument();
    expect(screen.queryByRole("menu")).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "切换文件视图" })).toHaveFocus();

    await chooseMode(user, "工作区模式");
    expect(screen.queryByRole("tree")).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "在中间打开选中文件" })).toBeDisabled();

    await chooseMode(user, "列表模式");
    expect(await screen.findByTestId("sftp-remote-entry-list")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "切换文件视图" }));
    expect(screen.getByRole("menuitemradio", { name: "列表模式" })).toBeChecked();
  });

  it("supports keyboard selection and dismisses without changing modes or stealing outside focus", async () => {
    const user = userEvent.setup();
    render(<SftpToolContent selectedMachine={sshMachine} />);
    await screen.findByTestId("sftp-remote-entry-list");
    const trigger = screen.getByRole("button", { name: "切换文件视图" });
    trigger.focus();
    await user.keyboard("{ArrowDown}");
    expect(screen.getByRole("menuitemradio", { name: "列表模式" })).toHaveFocus();
    await user.keyboard("{ArrowDown}{Enter}");
    expect(screen.getByRole("tree", { name: "SFTP 目录树" })).toBeInTheDocument();
    expect(trigger).toHaveFocus();

    await user.keyboard("{Enter}");
    expect(screen.getByRole("menuitemradio", { name: "树形模式" })).toBeChecked();
    expect(screen.getByRole("menuitemradio", { name: "树形模式" })).toHaveFocus();
    await user.keyboard("{End}");
    expect(screen.getByRole("menuitemradio", { name: "工作区模式" })).toHaveFocus();
    await user.keyboard("{Home}{ArrowUp}");
    expect(screen.getByRole("menuitemradio", { name: "工作区模式" })).toHaveFocus();
    await user.keyboard("{Escape}");
    expect(screen.queryByRole("menu")).not.toBeInTheDocument();
    expect(trigger).toHaveFocus();
    expect(screen.getByRole("tree", { name: "SFTP 目录树" })).toBeInTheDocument();

    await user.click(trigger);
    const path = screen.getByRole("textbox", { name: "当前远程路径" });
    await user.click(path);
    expect(screen.queryByRole("menu")).not.toBeInTheDocument();
    expect(path).toHaveFocus();
    await user.click(trigger);
    await user.tab();
    expect(screen.queryByRole("menu")).not.toBeInTheDocument();
    expect(screen.getByRole("tree", { name: "SFTP 目录树" })).toBeInTheDocument();
  });

  it("preserves expanded tree directories when switching browser modes", async () => {
    const user = userEvent.setup();

    render(<SftpToolContent selectedMachine={sshMachine} />);

    await screen.findByTestId("sftp-remote-entry-list");
    await chooseMode(user, "树形模式");

    const varNode = await screen.findByRole("treeitem", { name: "var" });
    await user.click(varNode);
    await waitFor(() =>
      expect(varNode).toHaveAttribute("aria-expanded", "true"),
    );
    expect(await screen.findByRole("treeitem", { name: "log" })).toBeInTheDocument();

    await chooseMode(user, "列表模式");
    await screen.findByTestId("sftp-remote-entry-list");
    await chooseMode(user, "树形模式");

    expect(
      await screen.findByRole("treeitem", { name: "var" }),
    ).toHaveAttribute("aria-expanded", "true");
    expect(screen.getByRole("treeitem", { name: "log" })).toBeInTheDocument();
  });

  it("applies the hidden-file toggle to tree nodes without reloading the tree", async () => {
    const user = userEvent.setup();

    render(<SftpToolContent selectedMachine={sshMachine} />);

    await screen.findByTestId("sftp-remote-entry-list");
    await chooseMode(user, "树形模式");

    expect(
      await screen.findByRole("treeitem", { name: ".env" }),
    ).toBeInTheDocument();
    const listCallCount = sftpApiMocks.listSftpDirectory.mock.calls.length;

    await user.click(screen.getByRole("button", { name: "隐藏隐藏文件" }));

    expect(
      screen.queryByRole("treeitem", { name: ".env" }),
    ).not.toBeInTheDocument();
    expect(sftpApiMocks.listSftpDirectory).toHaveBeenCalledTimes(listCallCount);

    await user.click(screen.getByRole("button", { name: "显示隐藏文件" }));

    expect(
      screen.getByRole("treeitem", { name: ".env" }),
    ).toBeInTheDocument();
    expect(sftpApiMocks.listSftpDirectory).toHaveBeenCalledTimes(listCallCount);
  });

  it("uploads files into the directory selected from the tree context menu", async () => {
    const user = userEvent.setup();

    render(<SftpToolContent selectedMachine={sshMachine} />);

    await screen.findByTestId("sftp-remote-entry-list");
    await chooseMode(user, "树形模式");

    const varNode = await screen.findByRole("treeitem", { name: "var" });
    fireEvent.contextMenu(varNode, { clientX: 32, clientY: 48 });
    await user.click(
      screen.getByRole("menuitem", { name: "上传文件到此目录" }),
    );

    await waitFor(() =>
      expect(sftpApiMocks.enqueueSftpTransfer).toHaveBeenCalledWith({
        conflictPolicy: "overwrite",
        direction: "upload",
        hostId: "prod-api",
        kind: "file",
        localPath: "/Users/me/release.tgz",
        remotePath: "/var/release.tgz",
      }),
    );
  });

  it("loads and selects the requested file when a workspace tab asks to reveal in SFTP", async () => {
    render(
      <SftpToolContent
        selectedMachine={sshMachine}
        sftpRevealRequest={{
          directoryPath: "/var/log",
          filePath: "/var/log/app.log",
          id: 1,
          target: { hostId: "prod-api", kind: "ssh" },
        }}
      />,
    );

    expect(await screen.findByText("app.log")).toBeInTheDocument();
    await waitFor(() =>
      expect(sftpApiMocks.listSftpDirectory).toHaveBeenLastCalledWith({
        hostId: "prod-api",
        path: "/var/log",
      }),
    );
    expect(
      screen
        .getByRole("button", { name: "文件 app.log" })
        .closest("[aria-selected='true']"),
    ).toBeInTheDocument();
  });
});
