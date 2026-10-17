package com.example.personnel.model.dto;

/** One raise on record, as the API answers it. */
public record HistoryEntry(String date, Integer oldSalary, Integer newSalary,
                           Integer amount, String reason) {
}
