package com.example.personnel.model.dto;

/** The request body of {@code POST /employees/raise}. */
public record RaiseIn(Integer amount, String reason) {
}
