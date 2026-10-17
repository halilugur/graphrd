package com.example.personnel.model.dto;

/** The request body of {@code POST /employees}. */
public record EmployeeIn(String name, String email, Integer age, Integer salary,
                         String hired, String company) {
}
