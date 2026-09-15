"use client";

import { useState, type ReactNode } from "react";
import { useCombobox } from "downshift";
import { InputResetButton } from "@/components/settings/field";

interface ComboboxItem {
  value: string;
  label: string;
}

interface ComboboxInputProps {
  label: ReactNode;
  description?: ReactNode;
  value: string;
  items: ComboboxItem[];
  onChange: (value: string) => void;
  onBlur?: () => void;
  placeholder?: string;
  allowFreeText?: boolean;
  disabled?: boolean;
  hideLabel?: boolean;
  onClear?: () => void;
  clearLabel?: string;
}

export function ComboboxInput({
  label,
  description,
  value,
  items,
  onChange,
  placeholder,
  allowFreeText = true,
  onBlur,
  disabled = false,
  hideLabel = false,
  onClear,
  clearLabel = "Reset to default",
}: ComboboxInputProps) {
  const [query, setQuery] = useState<string | null>(null);
  const selectedItem = items.find(item => item.value === value) ?? null;
  const displayValue = selectedItem?.label ?? value;
  const filteredItems = query
    ? items.filter(item => item.label.toLowerCase().includes(query.toLowerCase()) || item.value.toLowerCase().includes(query.toLowerCase()))
    : items;

  const {
    isOpen,
    getToggleButtonProps,
    getLabelProps,
    getMenuProps,
    getInputProps,
    getItemProps,
    highlightedIndex,
    closeMenu,
  } = useCombobox({
    items: filteredItems,
    inputValue: query ?? displayValue,
    selectedItem,
    itemToString: (item) => item?.label ?? "",
    itemToKey: (item) => item?.value ?? "",
    onInputValueChange: ({ inputValue, type }) => {
      if (type === useCombobox.stateChangeTypes.InputChange) {
        setQuery(inputValue ?? "");
        if (allowFreeText) onChange(inputValue ?? "");
      }
    },
    onSelectedItemChange: ({ selectedItem }) => {
      if (selectedItem) {
        onChange(selectedItem.value);
        setQuery(null);
      }
    },
    onStateChange: ({ type }) => {
      if (type === useCombobox.stateChangeTypes.InputBlur || type === useCombobox.stateChangeTypes.InputKeyDownEscape) {
        setQuery(null);
      }
    },
    onIsOpenChange: ({ isOpen: nowOpen }) => {
      if (!nowOpen) setQuery(null);
    },
  });

  return (
    <div className={hideLabel && !description ? undefined : "space-y-1"}>
      <label
        className={hideLabel ? "sr-only" : "flex items-center gap-2 text-sm font-medium text-text-secondary"}
        {...getLabelProps()}
      >
        {label}
      </label>
      {description && (
        <p className="text-xs text-text-tertiary">{description}</p>
      )}
      <div className="relative">
        <div className="flex">
          <input
            {...getInputProps({
              onBlur,
              disabled,
            })}
            placeholder={placeholder}
            disabled={disabled}
            className={`w-full rounded-lg border border-border bg-surface px-3 py-2 ${onClear ? "pr-14" : "pr-8"} text-sm text-text-primary placeholder:text-text-tertiary focus:border-accent focus:outline-none ${disabled ? "opacity-50 cursor-not-allowed" : ""}`}
          />
          {onClear && <InputResetButton label={clearLabel} className="absolute right-7 top-1/2 -translate-y-1/2"
            onClick={() => { setQuery(null); closeMenu(); onClear(); }} />}
          <button
            type="button"
            {...getToggleButtonProps({ disabled })}
            className="absolute right-2 top-1/2 -translate-y-1/2 text-text-tertiary hover:text-text-primary disabled:cursor-not-allowed disabled:opacity-50"
            aria-label="toggle menu"
          >
            <svg
              className={`h-4 w-4 transition-transform ${isOpen ? "rotate-180" : ""}`}
              fill="none"
              viewBox="0 0 24 24"
              stroke="currentColor"
              strokeWidth={2}
            >
              <path
                strokeLinecap="round"
                strokeLinejoin="round"
                d="M19 9l-7 7-7-7"
              />
            </svg>
          </button>
        </div>
        <ul
          {...getMenuProps()}
          className={`absolute z-10 mt-1 w-full max-h-60 overflow-y-auto rounded-lg border border-border bg-surface shadow-lg ${
            !(isOpen && !disabled && filteredItems.length > 0) ? "hidden" : ""
          }`}
        >
          {isOpen && !disabled &&
            filteredItems.map((item, index) => (
              <li
                key={item.value}
                {...getItemProps({ item, index })}
                className={`px-3 py-2 text-sm cursor-pointer ${
                  highlightedIndex === index
                    ? "bg-surface-tertiary text-text-primary"
                    : "text-text-primary"
                }`}
              >
                {item.label}
              </li>
            ))}
        </ul>
      </div>
    </div>
  );
}
