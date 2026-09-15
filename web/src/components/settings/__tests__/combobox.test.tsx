import React, { useState } from "react";
import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { ComboboxInput } from "../combobox";

const items = [
  { value: "api_key", label: "API key" },
  { value: "azure_entra", label: "Microsoft Entra ID" },
];

function Harness({ allowFreeText = false, onChange = () => {} }: { allowFreeText?: boolean; onChange?: (value: string) => void }) {
  const [value, setValue] = useState("api_key");
  return <ComboboxInput label="Authentication" items={items} value={value} allowFreeText={allowFreeText}
    onChange={next => { setValue(next); onChange(next); }} />;
}

describe("settings combobox", () => {
  it("searches labels without changing the saved value, then submits the selected identifier", () => {
    const onChange = vi.fn();
    render(<Harness onChange={onChange} />);
    const input = screen.getByRole("combobox", { name: "Authentication" });
    expect(input).toHaveValue("API key");
    fireEvent.change(input, { target: { value: "Micro" } });
    fireEvent.change(input, { target: { value: "Microsoft" } });
    expect(input).toHaveValue("Microsoft");
    expect(screen.queryByRole("option", { name: "API key" })).not.toBeInTheDocument();
    expect(onChange).not.toHaveBeenCalled();
    fireEvent.keyDown(input, { key: "ArrowDown" });
    fireEvent.keyDown(input, { key: "Enter" });
    expect(onChange).toHaveBeenLastCalledWith("azure_entra");
    expect(input).toHaveValue("Microsoft Entra ID");
  });

  it("restores the selected label when an unselected search is dismissed", () => {
    const onChange = vi.fn();
    render(<Harness onChange={onChange} />);
    const input = screen.getByRole("combobox", { name: "Authentication" });
    fireEvent.change(input, { target: { value: "unmatched" } });
    fireEvent.keyDown(input, { key: "Escape" });
    expect(input).toHaveValue("API key");
    fireEvent.change(input, { target: { value: "Microsoft" } });
    fireEvent.blur(input);
    expect(input).toHaveValue("API key");
    expect(onChange).not.toHaveBeenCalled();
  });

  it("preserves free-text input where the settings allow custom values", () => {
    const onChange = vi.fn();
    render(<Harness allowFreeText onChange={onChange} />);
    const input = screen.getByRole("combobox", { name: "Authentication" });
    fireEvent.change(input, { target: { value: "Custom value" } });
    fireEvent.blur(input);
    expect(input).toHaveValue("Custom value");
    expect(onChange).toHaveBeenLastCalledWith("Custom value");
  });

  it("updates option labels and disables both the input and menu button", () => {
    const onChange = vi.fn();
    const view = render(<ComboboxInput label="Authentication" value="api_key" items={items} onChange={onChange} />);
    view.rerender(<ComboboxInput label="Authentication" value="api_key" items={[{ value: "api_key", label: "Saved API key" }, items[1]]} onChange={onChange} disabled />);
    expect(screen.getByRole("combobox", { name: "Authentication" })).toHaveValue("Saved API key");
    expect(screen.getByRole("combobox")).toBeDisabled();
    expect(screen.getByRole("button", { name: "toggle menu" })).toBeDisabled();
    fireEvent.click(screen.getByRole("button", { name: "toggle menu" }));
    expect(screen.queryByRole("option")).not.toBeInTheDocument();
  });
});
